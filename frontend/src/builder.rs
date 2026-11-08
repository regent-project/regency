//! Attribute builder (D23 one-shot; user's request, like Grafana's query
//! "builder" next to its "code" view): forms generated from regent-sdk's
//! JSON Schema of an `Attribute` (feature `schema`, served by backend's
//! `GetAttributeSchema`), so they follow the SDK's types. The value edited is
//! serde's JSON form of a list of attributes; backend's `ConvertAttributes`
//! turns it into the SDK's YAML (and validates it).
//!
//! Schema shapes (as schemars writes them) and their widgets:
//! - `oneOf` / `anyOf` / `enum`: a picker (externally tagged variants are
//!   named after their key, internally tagged ones after their tag's `const`);
//!   choosing fills in the variant's defaults;
//! - object: required properties shown, optional ones added on demand; with
//!   a `oneOf` as well (a flattened enum), both apply to the same object;
//! - `additionalProperties` only: key / value rows;
//! - array: items added and removed;
//! - string (with its `pattern`), integer, number, boolean: inputs;
//! - anything else: that part as raw JSON.

use std::sync::Arc;

use leptos::prelude::*;
use serde_json::{Map, Value, json};

/// A step into a JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

pub type Path = Vec<Seg>;

fn with(path: &Path, seg: Seg) -> Path {
    let mut path = path.clone();
    path.push(seg);
    path
}

pub fn get_at<'a>(value: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, seg| match seg {
        Seg::Key(k) => v.get(k),
        Seg::Index(i) => v.get(*i),
    })
}

fn child_mut<'a>(value: &'a mut Value, seg: &Seg) -> &'a mut Value {
    match seg {
        Seg::Key(k) => {
            if !value.is_object() {
                *value = json!({});
            }
            let map = value.as_object_mut().expect("object");
            map.entry(k.clone()).or_insert(Value::Null)
        }
        Seg::Index(i) => {
            if !value.is_array() {
                *value = json!([]);
            }
            let items = value.as_array_mut().expect("array");
            while items.len() <= *i {
                items.push(Value::Null);
            }
            &mut items[*i]
        }
    }
}

pub fn set_at(value: &mut Value, path: &[Seg], new: Value) {
    let mut current = value;
    for seg in path {
        current = child_mut(current, seg);
    }
    *current = new;
}

pub fn remove_at(value: &mut Value, path: &[Seg]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = value;
    for seg in parents {
        current = child_mut(current, seg);
    }
    match last {
        Seg::Key(k) => {
            if let Some(map) = current.as_object_mut() {
                map.remove(k);
            }
        }
        Seg::Index(i) => {
            if let Some(items) = current.as_array_mut()
                && *i < items.len()
            {
                items.remove(*i);
            }
        }
    }
}

/// Without the `null`s serde writes for unset options: in the builder, a
/// field is shown when its key is present (even blank), so unset options
/// must be absent.
pub fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

/// Attribute natures (the `Detail` variants) by category, as regent-sdk
/// organizes them (its `state::attribute::<category>` modules, 0.23.0). A
/// nature missing here is listed under "other" (see the test against the
/// schema fixture).
pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("ai", &["Ollama", "Onnx"]),
    ("container", &["Apptainer", "Docker", "Enroot", "Podman"]),
    (
        "gpu",
        &["AppleMps", "NvidiaContainerToolkit", "NvidiaDriver"],
    ),
    ("network", &["Dhcp", "Dns", "Iptables"]),
    (
        "observability",
        &["Alloy", "FluentBit", "NodeExporter", "OtelCollector"],
    ),
    (
        "package",
        &["Apt", "AptRepo", "DnfRepo", "Pacman", "YumDnf"],
    ),
    ("shell", &["Command"]),
    (
        "system",
        &["Cron", "Group", "Hostname", "Service", "Storage", "User"],
    ),
    ("utilities", &["Debug", "LineInFile", "Ping", "Template"]),
];

/// The SDK's key of an attribute's nature.
const NATURE_KEY: &str = "Detail";

/// How a property is named in the builder (the data keeps the SDK's key).
pub fn display_name(key: &str) -> &str {
    if key == NATURE_KEY { "Nature" } else { key }
}

/// The category of a nature, `"other"` if unknown.
pub fn category_of(nature: &str) -> &'static str {
    CATEGORIES
        .iter()
        .find(|(_, natures)| natures.contains(&nature))
        .map_or("other", |(category, _)| category)
}

/// One choice of a picker.
#[derive(Clone, Debug)]
pub struct Alt {
    pub label: String,
    pub schema: Value,
    pub description: Option<String>,
}

/// An attribute's JSON Schema (with its `$defs`).
pub struct Schema {
    root: Value,
}

impl Schema {
    pub fn new(root: Value) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Value {
        &self.root
    }

    pub fn resolve<'a>(&'a self, schema: &'a Value) -> &'a Value {
        let mut current = schema;
        for _ in 0..16 {
            let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
                break;
            };
            let name = reference.trim_start_matches("#/$defs/");
            match self.root.get("$defs").and_then(|d| d.get(name)) {
                Some(target) => current = target,
                None => break,
            }
        }
        current
    }

    /// The choices of a `oneOf` / `anyOf` / `enum` node, `null` excluded (an
    /// optional value is added or removed instead); an alternative that is
    /// an enum of several strings becomes one choice per string.
    pub fn alternatives(&self, schema: &Value) -> Option<Vec<Alt>> {
        let schema = self.resolve(schema);
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return Some(values.iter().map(constant).collect());
        }
        let alts = ["oneOf", "anyOf"]
            .iter()
            .find_map(|key| schema.get(*key).and_then(Value::as_array))?;
        let mut out = Vec::new();
        for alt in alts {
            let resolved = self.resolve(alt);
            if is_null(resolved) {
                continue;
            }
            match resolved.get("enum").and_then(Value::as_array) {
                Some(values) if values.len() > 1 => out.extend(values.iter().map(constant)),
                _ => out.push(Alt {
                    label: self.label(alt),
                    schema: alt.clone(),
                    description: description(resolved),
                }),
            }
        }
        Some(out)
    }

    fn label(&self, schema: &Value) -> String {
        let resolved = self.resolve(schema);
        if let Some(c) = resolved.get("const") {
            return text_of(c);
        }
        if let Some([only]) = resolved
            .get("enum")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
        {
            return text_of(only);
        }
        if let Some(key) = external_tag(resolved) {
            return key;
        }
        if let Some((_, value)) = internal_tag(resolved) {
            return value;
        }
        if let Some(name) = schema
            .get("$ref")
            .and_then(Value::as_str)
            .map(|r| r.trim_start_matches("#/$defs/"))
        {
            return name.to_string();
        }
        if let Some(title) = resolved.get("title").and_then(Value::as_str) {
            return title.to_string();
        }
        // an untagged object variant: named after its fields
        if let Some(props) = resolved.get("properties").and_then(Value::as_object) {
            let required = required(resolved);
            let names: Vec<&str> = if required.is_empty() {
                props.keys().map(String::as_str).collect()
            } else {
                required.iter().map(String::as_str).collect()
            };
            if !names.is_empty() {
                return names.join(" + ");
            }
        }
        match type_of(resolved) {
            Some("string") => "text",
            Some("integer") | Some("number") => "number",
            Some("boolean") => "yes / no",
            Some("array") => "list",
            Some("object") => "fields",
            _ => "value",
        }
        .to_string()
    }

    /// Does `value` have the shape of `schema` (enough to tell variants
    /// apart)?
    pub fn matches(&self, schema: &Value, value: &Value) -> bool {
        let schema = self.resolve(schema);
        if let Some(c) = schema.get("const") {
            return value == c;
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values.contains(value);
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(alts) = schema.get(key).and_then(Value::as_array)
                && schema.get("properties").is_none()
            {
                return alts.iter().any(|alt| self.matches(alt, value));
            }
        }
        if let Some(key) = external_tag(schema) {
            return value.as_object().is_some_and(|o| o.contains_key(&key));
        }
        if let Some((key, tag)) = internal_tag(schema) {
            return value.get(&key).and_then(Value::as_str) == Some(tag.as_str());
        }
        match schema.get("type") {
            Some(Value::Array(types)) => types
                .iter()
                .filter_map(Value::as_str)
                .any(|t| has_type(t, value)),
            Some(Value::String(t)) => has_type(t, value),
            _ if schema.get("properties").is_some() => value.is_object(),
            _ => true,
        }
    }

    /// Index of the choice `value` is, if any.
    pub fn which(&self, alts: &[Alt], value: &Value) -> Option<usize> {
        alts.iter().position(|alt| self.matches(&alt.schema, value))
    }

    /// The empty skeleton of `schema`, for the user to fill in: required
    /// fields present but blank, choices not made (`null`, shown as
    /// "choose…"), except the tags that say which choice this is (a
    /// variant's key, an internal tag's constant). Nothing is pre-filled.
    pub fn blank_for(&self, schema: &Value) -> Value {
        self.blank_at(schema, 0)
    }

    fn blank_at(&self, schema: &Value, depth: usize) -> Value {
        let schema = self.resolve(schema);
        if depth > 24 {
            return Value::Null;
        }
        if let Some(c) = schema.get("const") {
            return c.clone();
        }
        if schema.get("enum").is_some() {
            return Value::Null;
        }
        let is_choice = ["oneOf", "anyOf"]
            .iter()
            .any(|key| schema.get(*key).is_some());
        if is_choice && schema.get("properties").is_none() {
            return Value::Null;
        }
        match type_of(schema) {
            Some("array") => json!([]),
            Some("string") => json!(""),
            Some("boolean") => json!(false),
            Some("integer") | Some("number") => Value::Null,
            Some("object") => self.object_blank(schema, depth),
            _ if schema.get("properties").is_some() => self.object_blank(schema, depth),
            _ => Value::Null,
        }
    }

    /// Required properties, blank (a flattened enum's choice is not made).
    fn object_blank(&self, schema: &Value, depth: usize) -> Value {
        let mut out = Map::new();
        let required = required(schema);
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (key, prop) in props {
                if required.contains(key) {
                    out.insert(key.clone(), self.blank_at(prop, depth + 1));
                }
            }
        }
        Value::Object(out)
    }

    /// What the user still has to fill in, as readable paths (`Privilege`,
    /// `LineInFile › State`): required values left blank, choices not made.
    pub fn missing(&self, schema: &Value, value: &Value) -> Vec<String> {
        let mut out = Vec::new();
        self.missing_at(schema, value, "", &mut out, 0);
        out
    }

    fn missing_at(
        &self,
        schema: &Value,
        value: &Value,
        path: &str,
        out: &mut Vec<String>,
        depth: usize,
    ) {
        let schema = self.resolve(schema);
        if depth > 24 || schema.get("const").is_some() {
            return;
        }
        let join = |key: &str| {
            let key = display_name(key);
            if path.is_empty() {
                key.to_string()
            } else {
                format!("{path} › {key}")
            }
        };
        if schema.get("properties").is_none()
            && let Some(alts) = self.alternatives(schema)
        {
            let Some(index) = (!value.is_null())
                .then(|| self.which(&alts, value))
                .flatten()
            else {
                out.push(if path.is_empty() {
                    "the choice".to_string()
                } else {
                    path.to_string()
                });
                return;
            };
            let alt = self.resolve(&alts[index].schema);
            if value.as_str() == Some("") {
                // a text chosen but left empty
                out.push(if path.is_empty() {
                    "the value".to_string()
                } else {
                    path.to_string()
                });
                return;
            }
            if let Some(key) = external_tag(alt) {
                let inner = &alt["properties"][&key];
                let child = value.get(&key).unwrap_or(&Value::Null);
                self.missing_at(inner, child, &join(&key), out, depth + 1);
            } else if alt.get("properties").is_some() {
                self.missing_at(alt, value, path, out, depth + 1);
            }
            return;
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for key in required(schema) {
                let Some(prop) = props.get(&key) else {
                    continue;
                };
                if self.resolve(prop).get("const").is_some() {
                    continue;
                }
                match value.get(&key) {
                    None | Some(Value::Null) => out.push(join(&key)),
                    Some(Value::String(text)) if text.is_empty() => out.push(join(&key)),
                    Some(child) => self.missing_at(prop, child, &join(&key), out, depth + 1),
                }
            }
            // a flattened enum next to the properties
            if let Some(alts) = schema
                .get("oneOf")
                .and_then(|o| self.alternatives(&json!({ "oneOf": o })))
            {
                match self.which(&alts, value) {
                    Some(index) => {
                        let alt = self.resolve(&alts[index].schema).clone();
                        self.missing_at(&alt, value, path, out, depth + 1);
                    }
                    None => out.push(join("(choice)")),
                }
            }
            return;
        }
        if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
            for (i, item) in values.iter().enumerate() {
                self.missing_at(items, item, &join(&format!("#{}", i + 1)), out, depth + 1);
            }
        }
    }

    /// The properties of every choice of a flattened enum (removed when the
    /// choice changes).
    fn variant_keys(&self, alts: &[Alt]) -> Vec<String> {
        alts.iter()
            .filter_map(|alt| {
                self.resolve(&alt.schema)
                    .get("properties")
                    .and_then(Value::as_object)
            })
            .flat_map(|props| props.keys().cloned())
            .collect()
    }
}

fn constant(value: &Value) -> Alt {
    Alt {
        label: text_of(value),
        schema: json!({ "const": value }),
        description: None,
    }
}

fn text_of(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn is_null(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
        || schema.get("const") == Some(&Value::Null)
}

fn has_type(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// The type of a node, ignoring `null` in a list of types.
fn type_of(schema: &Value) -> Option<&str> {
    match schema.get("type")? {
        Value::String(t) => Some(t),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null"),
        _ => None,
    }
}

fn required(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `{"Apt": {..}}`: an externally tagged variant, named after its only key.
fn external_tag(schema: &Value) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    let required = required(schema);
    (props.len() == 1 && required.len() == 1 && props.contains_key(&required[0]))
        .then(|| required[0].clone())
}

/// `{"State": "Present", ..}`: an internally tagged variant, named after its
/// tag's constant.
fn internal_tag(schema: &Value) -> Option<(String, String)> {
    schema
        .get("properties")?
        .as_object()?
        .iter()
        .find_map(|(key, prop)| Some((key.clone(), prop.get("const")?.as_str()?.to_string())))
}

fn description(schema: &Value) -> Option<String> {
    schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The first paragraph of a description, for the help line under a field.
fn summary(description: &str) -> String {
    description
        .split("\n\n")
        .next()
        .unwrap_or("")
        .replace('\n', " ")
}

#[derive(Clone)]
struct Ctx {
    schema: Arc<Schema>,
    data: RwSignal<Value>,
}

/// The builder: a list of attributes (`data`, serde's JSON form).
#[component]
pub fn AttributeBuilder(schema: Arc<Schema>, data: RwSignal<Value>) -> impl IntoView {
    let ctx = Ctx {
        schema: schema.clone(),
        data,
    };
    let count = Memo::new(move |_| data.with(|d| d.as_array().map_or(0, Vec::len)));
    let add = {
        let schema = schema.clone();
        move |_| {
            let fresh = schema.blank_for(schema.root());
            data.update(|d| {
                if !d.is_array() {
                    *d = json!([]);
                }
                d.as_array_mut().expect("array").push(fresh.clone());
            });
        }
    };
    view! {
        <div class="builder">
            {move || {
                (0..count.get())
                    .map(|i| attribute_card(ctx.clone(), i, count.get()))
                    .collect::<Vec<_>>()
            }}
            <button type="button" class="b-add" on:click=add>"+ Add attribute"</button>
        </div>
    }
}

fn attribute_card(ctx: Ctx, index: usize, count: usize) -> AnyView {
    let data = ctx.data;
    let path: Path = vec![Seg::Index(index)];
    let title = Memo::new(move |_| {
        data.with(|d| {
            let attribute = d.get(index);
            let kind = attribute
                .and_then(|a| a.get("Detail"))
                .and_then(Value::as_object)
                .and_then(|o| o.keys().next().cloned())
                .unwrap_or_else(|| "attribute".into());
            let name = attribute
                .and_then(|a| a.get("Name"))
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(|n| format!(" — {n}"))
                .unwrap_or_default();
            format!("{} · {kind}{name}", index + 1)
        })
    });
    let root = ctx.schema.root().clone();
    let body = field(ctx.clone(), root, path.clone(), String::new(), true);
    view! {
        <fieldset class="b-attribute">
            <legend>
                <span>{move || title.get()}</span>
                {(count > 1).then(|| view! {
                    <button type="button" class="b-remove" title="Remove this attribute"
                        on:click=move |_| data.update(|d| remove_at(d, &[Seg::Index(index)]))>"×"</button>
                })}
            </legend>
            {body}
        </fieldset>
    }
    .into_any()
}

/// The widget of `schema` at `path`.
fn field(ctx: Ctx, schema: Value, path: Path, label: String, required: bool) -> AnyView {
    let resolved = ctx.schema.resolve(&schema).clone();
    let help = description(&resolved);
    if resolved.get("properties").is_none()
        && let Some(alts) = ctx.schema.alternatives(&resolved)
    {
        return choice(ctx, alts, path, label, help, false, required);
    }
    let kind = type_of(&resolved)
        .map(str::to_string)
        .or_else(|| resolved.get("properties").map(|_| "object".to_string()));
    match kind.as_deref() {
        Some("object") => {
            let fields = object_fields(ctx, resolved, path, &[]);
            if label.is_empty() {
                fields
            } else {
                view! {
                    <div class="b-group">
                        {head(&label, required, help.as_deref())}
                        <div class="b-nested">{fields}</div>
                    </div>
                }
                .into_any()
            }
        }
        Some("array") => list(ctx, resolved, path, label, required, help),
        Some("string") | Some("integer") | Some("number") | Some("boolean") => {
            scalar(ctx, resolved, path, label, required, help)
        }
        _ => raw_json(ctx, path, label, help),
    }
}

fn head(label: &str, required: bool, help: Option<&str>) -> AnyView {
    if label.is_empty() {
        return ().into_any();
    }
    let label = display_name(label);
    let full = help.unwrap_or_default().to_string();
    let short = help.map(summary).filter(|s| !s.is_empty());
    view! {
        <div class="b-head" title=full>
            <span class="b-label">{label.to_string()}{required.then_some(" *")}</span>
            {short.map(|s| view! { <span class="b-help">{s}</span> })}
        </div>
    }
    .into_any()
}

/// A picker; `merged`: the choice's properties live in the enclosing object
/// (a flattened enum), next to the object's own.
fn choice(
    ctx: Ctx,
    alts: Vec<Alt>,
    path: Path,
    label: String,
    help: Option<String>,
    merged: bool,
    required: bool,
) -> AnyView {
    let alts = Arc::new(alts);
    let data = ctx.data;
    let current = {
        let (schema, alts, path) = (ctx.schema.clone(), alts.clone(), path.clone());
        Memo::new(move |_| data.with(|d| get_at(d, &path).and_then(|v| schema.which(&alts, v))))
    };
    let pick = {
        let (schema, alts, path) = (ctx.schema.clone(), alts.clone(), path.clone());
        move |index: usize| {
            let Some(alt) = alts.get(index) else { return };
            let fresh = schema.blank_for(&alt.schema);
            if merged {
                let drop = schema.variant_keys(&alts);
                data.update(|d| {
                    let mut object = get_at(d, &path).cloned().unwrap_or_else(|| json!({}));
                    if let Some(map) = object.as_object_mut() {
                        map.retain(|k, _| !drop.contains(k));
                        if let Value::Object(extra) = fresh {
                            map.extend(extra);
                        }
                    }
                    set_at(d, &path, object);
                });
            } else {
                data.update(|d| set_at(d, &path, fresh));
            }
        }
    };
    let option = move |i: usize, alt: &Alt| {
        let title = alt.description.clone().unwrap_or_default();
        view! { <option value=i.to_string() title=title selected=move || current.get() == Some(i)>{alt.label.clone()}</option> }
    };
    // the nature picker: grouped by category, like the SDK's modules
    let natures = path.last() == Some(&Seg::Key(NATURE_KEY.to_string()));
    let options = if natures {
        let mut groups: Vec<(&str, Vec<(usize, &Alt)>)> = Vec::new();
        for category in CATEGORIES.iter().map(|(c, _)| *c).chain(["other"]) {
            let mut members: Vec<(usize, &Alt)> = alts
                .iter()
                .enumerate()
                .filter(|(_, alt)| category_of(&alt.label) == category)
                .collect();
            members.sort_by(|a, b| a.1.label.cmp(&b.1.label));
            if !members.is_empty() {
                groups.push((category, members));
            }
        }
        groups
            .into_iter()
            .map(|(category, members)| {
                let items = members
                    .into_iter()
                    .map(|(i, alt)| option(i, alt))
                    .collect::<Vec<_>>();
                view! { <optgroup label=category>{items}</optgroup> }.into_any()
            })
            .collect::<Vec<_>>()
    } else {
        alts.iter()
            .enumerate()
            .map(|(i, alt)| option(i, alt).into_any())
            .collect::<Vec<_>>()
    };
    let body = {
        let (ctx, alts, path) = (ctx.clone(), alts.clone(), path.clone());
        move || {
            current
                .get()
                .map(|i| variant_body(ctx.clone(), &alts[i].schema, path.clone(), merged))
        }
    };
    let chosen_help = {
        let alts = alts.clone();
        move || {
            current
                .get()
                .and_then(|i| alts[i].description.clone())
                .map(|d| view! { <span class="b-help" title=d.clone()>{summary(&d)}</span> })
        }
    };
    view! {
        <div class="b-choice">
            {head(&label, required, help.as_deref())}
            <div class="b-row">
                <select on:change=move |ev| {
                    if let Ok(i) = event_target_value(&ev).parse::<usize>() {
                        pick(i);
                    }
                }>
                    <option value="" disabled=true selected=move || current.get().is_none()>"choose…"</option>
                    {options}
                </select>
                {chosen_help}
            </div>
            <div class="b-nested">{body}</div>
        </div>
    }
    .into_any()
}

/// What a chosen variant asks for.
fn variant_body(ctx: Ctx, schema: &Value, path: Path, merged: bool) -> AnyView {
    let resolved = ctx.schema.resolve(schema).clone();
    if resolved.get("const").is_some() || resolved.get("enum").is_some() {
        return ().into_any();
    }
    if !merged && let Some(key) = external_tag(&resolved) {
        let inner = resolved["properties"][&key].clone();
        return field(ctx, inner, with(&path, Seg::Key(key)), String::new(), true);
    }
    if resolved.get("properties").is_some() {
        let tag = internal_tag(&resolved).map(|(k, _)| k);
        return object_fields(ctx, resolved, path, tag.as_slice());
    }
    field(ctx, resolved, path, String::new(), true)
}

/// The properties of an object (`skip`: tags shown by a picker), its
/// optional ones on demand, and its flattened enum if any.
fn object_fields(ctx: Ctx, schema: Value, path: Path, skip: &[String]) -> AnyView {
    let data = ctx.data;
    let required = required(&schema);
    let props: Vec<(String, Value)> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| {
            p.iter()
                .filter(|(k, v)| !skip.contains(k) && v.get("const").is_none())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    // present = the key is there, even blank (see strip_nulls)
    let present = {
        let path = path.clone();
        Memo::new(move |_| {
            data.with(|d| {
                get_at(d, &path)
                    .and_then(Value::as_object)
                    .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
        })
    };
    // in the schema's order, `Name` first (an attribute's name reads best on top)
    let mut ordered = props.clone();
    ordered.sort_by_key(|(k, _)| k != "Name");
    let ordered: Arc<Vec<(String, Value, bool)>> = Arc::new(
        ordered
            .into_iter()
            .map(|(k, v)| {
                let mandatory = required.contains(&k);
                (k, v, mandatory)
            })
            .collect(),
    );
    // re-rendered only when the set of present optional fields changes
    let shown_optional = {
        let ordered = ordered.clone();
        Memo::new(move |_| {
            let keys = present.get();
            ordered
                .iter()
                .filter(|(k, _, mandatory)| !mandatory && keys.contains(k))
                .map(|(k, _, _)| k.clone())
                .collect::<Vec<_>>()
        })
    };
    let fields = {
        let (ctx, ordered, path) = (ctx.clone(), ordered.clone(), path.clone());
        move || {
            let optional = shown_optional.get();
            ordered
                .iter()
                .filter(|(k, _, mandatory)| *mandatory || optional.contains(k))
                .map(|(k, v, mandatory)| {
                    let field_path = with(&path, Seg::Key(k.clone()));
                    let widget = field(ctx.clone(), v.clone(), field_path.clone(), k.clone(), *mandatory);
                    if *mandatory {
                        widget
                    } else {
                        view! {
                            <div class="b-optional">
                                {widget}
                                <button type="button" class="b-remove" title=format!("Remove {k}")
                                    on:click=move |_| data.update(|d| remove_at(d, &field_path))>"×"</button>
                            </div>
                        }
                        .into_any()
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    let addable = {
        let (schema_ref, ordered, path) = (ctx.schema.clone(), ordered.clone(), path.clone());
        move || {
            let keys = present.get();
            ordered
                .iter()
                .filter(|(k, _, mandatory)| !mandatory && !keys.contains(k))
                .map(|(k, v, _)| {
                    let fresh = schema_ref.blank_for(v);
                    let target = with(&path, Seg::Key(k.clone()));
                    let title = description(schema_ref.resolve(v)).unwrap_or_default();
                    view! {
                        <button type="button" class="b-chip" title=title
                            on:click=move |_| data.update(|d| set_at(d, &target, fresh.clone()))>
                            {format!("+ {k}")}
                        </button>
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    // a map: additionalProperties without properties
    let map = (props.is_empty() && schema.get("properties").is_none())
        .then(|| {
            schema
                .get("additionalProperties")
                .filter(|a| a.is_object())
                .cloned()
        })
        .flatten()
        .map(|values| key_values(ctx.clone(), values, path.clone()));
    // a flattened enum next to the properties
    let flattened =
        schema
            .get("properties")
            .and(ctx.schema.alternatives(
                &json!({ "oneOf": schema.get("oneOf").cloned().unwrap_or(Value::Null) }),
            ))
            .filter(|alts| !alts.is_empty())
            .map(|alts| {
                choice(
                    ctx.clone(),
                    alts,
                    path.clone(),
                    String::new(),
                    None,
                    true,
                    true,
                )
            });
    view! {
        {fields}
        {flattened}
        {map}
        <div class="b-chips">{addable}</div>
    }
    .into_any()
}

/// Key / value rows (`additionalProperties`).
fn key_values(ctx: Ctx, values: Value, path: Path) -> AnyView {
    let data = ctx.data;
    let keys = {
        let path = path.clone();
        Memo::new(move |_| {
            data.with(|d| {
                get_at(d, &path)
                    .and_then(Value::as_object)
                    .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
            })
        })
    };
    let rows = {
        let (ctx, values, path) = (ctx.clone(), values.clone(), path.clone());
        move || {
            keys.get()
                .into_iter()
                .map(|key| {
                    let old = key.clone();
                    let rename_path = path.clone();
                    let remove_path = with(&path, Seg::Key(key.clone()));
                    view! {
                        <div class="b-kv">
                            <input type="text" class="b-key" value=key.clone()
                                on:change=move |ev| {
                                    let new = event_target_value(&ev);
                                    if new.is_empty() || new == old { return; }
                                    data.update(|d| {
                                        if let Some(map) = get_at(d, &rename_path).cloned().and_then(|v| v.as_object().cloned()) {
                                            let renamed: Map<String, Value> = map.into_iter()
                                                .map(|(k, v)| (if k == old { new.clone() } else { k }, v))
                                                .collect();
                                            set_at(d, &rename_path, Value::Object(renamed));
                                        }
                                    });
                                } />
                            {field(ctx.clone(), values.clone(), with(&path, Seg::Key(key.clone())), String::new(), true)}
                            <button type="button" class="b-remove" on:click=move |_| data.update(|d| remove_at(d, &remove_path))>"×"</button>
                        </div>
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    let add = {
        let (schema, path) = (ctx.schema.clone(), path.clone());
        move |_| {
            let fresh = schema.blank_for(&values);
            data.update(|d| {
                let count = get_at(d, &path)
                    .and_then(Value::as_object)
                    .map_or(0, Map::len);
                set_at(
                    d,
                    &with(&path, Seg::Key(format!("key{}", count + 1))),
                    fresh.clone(),
                );
            });
        }
    };
    view! {
        <div class="b-map">{rows}<button type="button" class="b-chip" on:click=add>"+ entry"</button></div>
    }
    .into_any()
}

fn list(
    ctx: Ctx,
    schema: Value,
    path: Path,
    label: String,
    required: bool,
    help: Option<String>,
) -> AnyView {
    let data = ctx.data;
    let items = schema.get("items").cloned().unwrap_or(Value::Null);
    let count = {
        let path = path.clone();
        Memo::new(move |_| {
            data.with(|d| {
                get_at(d, &path)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            })
        })
    };
    let rows = {
        let (ctx, items, path) = (ctx.clone(), items.clone(), path.clone());
        move || {
            (0..count.get())
                .map(|i| {
                    let item_path = with(&path, Seg::Index(i));
                    let remove_path = item_path.clone();
                    view! {
                        <div class="b-item">
                            {field(ctx.clone(), items.clone(), item_path, String::new(), true)}
                            <button type="button" class="b-remove" on:click=move |_| data.update(|d| remove_at(d, &remove_path))>"×"</button>
                        </div>
                    }
                })
                .collect::<Vec<_>>()
        }
    };
    let add = {
        let (schema, path) = (ctx.schema.clone(), path.clone());
        move |_| {
            let fresh = schema.blank_for(&items);
            data.update(|d| {
                let length = get_at(d, &path)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                set_at(d, &with(&path, Seg::Index(length)), fresh.clone());
            });
        }
    };
    view! {
        <div class="b-group">
            {head(&label, required, help.as_deref())}
            <div class="b-list">{rows}<button type="button" class="b-chip" on:click=add>"+ item"</button></div>
        </div>
    }
    .into_any()
}

fn scalar(
    ctx: Ctx,
    schema: Value,
    path: Path,
    label: String,
    required: bool,
    help: Option<String>,
) -> AnyView {
    let data = ctx.data;
    let kind = type_of(&schema).unwrap_or("string").to_string();
    let nullable =
        matches!(schema.get("type"), Some(Value::Array(t)) if t.contains(&json!("null")));
    let read = {
        let path = path.clone();
        move || data.with(|d| get_at(d, &path).cloned().unwrap_or(Value::Null))
    };
    let input = match kind.as_str() {
        "boolean" => {
            let path = path.clone();
            view! {
                <input type="checkbox" prop:checked=move || read().as_bool().unwrap_or(false)
                    on:change=move |ev| data.update(|d| set_at(d, &path, json!(event_target_checked(&ev)))) />
            }
            .into_any()
        }
        "integer" | "number" => {
            let path = path.clone();
            let integer = kind == "integer";
            let min = schema.get("minimum").map(|m| m.to_string());
            let max = schema.get("maximum").map(|m| m.to_string());
            view! {
                <input type="number" step=if integer { "1" } else { "any" } min=min max=max
                    prop:value=move || match read() { Value::Number(n) => n.to_string(), _ => String::new() }
                    on:input=move |ev| {
                        let text = event_target_value(&ev);
                        let value = if text.trim().is_empty() {
                            if nullable { Value::Null } else { return }
                        } else if integer {
                            match text.parse::<i64>() { Ok(n) => json!(n), Err(_) => return }
                        } else {
                            match text.parse::<f64>() { Ok(n) => json!(n), Err(_) => return }
                        };
                        data.update(|d| set_at(d, &path, value));
                    } />
            }
            .into_any()
        }
        _ => {
            let path = path.clone();
            let pattern = schema
                .get("pattern")
                .and_then(Value::as_str)
                .map(str::to_string);
            view! {
                <input type="text" pattern=pattern spellcheck="false"
                    prop:value=move || match read() { Value::String(s) => s, Value::Null => String::new(), other => other.to_string() }
                    on:input=move |ev| {
                        let text = event_target_value(&ev);
                        let value = if text.is_empty() && nullable { Value::Null } else { Value::String(text) };
                        data.update(|d| set_at(d, &path, value));
                    } />
            }
            .into_any()
        }
    };
    view! {
        <div class="b-field">
            {head(&label, required, help.as_deref())}
            {input}
        </div>
    }
    .into_any()
}

/// A part the builder has no widget for: its JSON.
fn raw_json(ctx: Ctx, path: Path, label: String, help: Option<String>) -> AnyView {
    let data = ctx.data;
    let invalid = RwSignal::new(false);
    let text = {
        let path = path.clone();
        move || {
            data.with(|d| {
                get_at(d, &path)
                    .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                    .unwrap_or_default()
            })
        }
    };
    view! {
        <div class="b-field">
            {head(&label, false, help.as_deref())}
            <textarea class="b-json" class:invalid=move || invalid.get() rows="3" spellcheck="false"
                prop:value=text
                on:change=move |ev| {
                    match serde_json::from_str::<Value>(&event_target_value(&ev)) {
                        Ok(value) => { invalid.set(false); data.update(|d| set_at(d, &path, value)); }
                        Err(_) => invalid.set(true),
                    }
                }></textarea>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// regent-sdk 0.23.0's attribute schema (feature `schema`, what backend
    /// serves); regenerate it when regency's SDK pin moves.
    fn schema() -> Schema {
        Schema::new(serde_json::from_str(include_str!("testdata/attribute.schema.json")).unwrap())
    }

    #[test]
    fn paths() {
        let mut v = json!({});
        set_at(&mut v, &[Seg::Key("a".into()), Seg::Index(1)], json!(3));
        assert_eq!(v, json!({"a": [null, 3]}));
        assert_eq!(
            get_at(&v, &[Seg::Key("a".into()), Seg::Index(1)]),
            Some(&json!(3))
        );
        remove_at(&mut v, &[Seg::Key("a".into()), Seg::Index(0)]);
        assert_eq!(v, json!({"a": [3]}));
    }

    #[test]
    fn every_attribute_type_is_a_choice() {
        let schema = schema();
        let detail = &schema.root()["properties"]["Detail"];
        let alts = schema.alternatives(detail).unwrap();
        assert_eq!(alts.len(), 32);
        let labels: Vec<&str> = alts.iter().map(|a| a.label.as_str()).collect();
        assert!(
            labels.contains(&"LineInFile") && labels.contains(&"Iptables"),
            "{labels:?}"
        );
        // the current choice is found from the value
        let value = json!({"LineInFile": {"FilePath": "/etc/motd"}});
        let index = schema.which(&alts, &value).unwrap();
        assert_eq!(alts[index].label, "LineInFile");
    }

    #[test]
    fn blanks_choose_nothing_for_the_user() {
        let schema = schema();
        // a fresh attribute: its required choices exist, none made
        let attribute = schema.blank_for(schema.root());
        assert_eq!(attribute, json!({"Privilege": null, "Detail": null}));
        // picking LineInFile: its fields, blank; its choices not made
        let detail = &schema.root()["properties"]["Detail"];
        let alts = schema.alternatives(detail).unwrap();
        let line = alts.iter().find(|a| a.label == "LineInFile").unwrap();
        let value = schema.blank_for(&line.schema);
        assert_eq!(
            value,
            json!({"LineInFile": {"FilePath": "", "State": null, "Line": null}})
        );
        // the picked variant is still recognised from its blank value
        assert_eq!(
            schema.which(&alts, &value).map(|i| alts[i].label.as_str()),
            Some("LineInFile")
        );
    }

    #[test]
    fn flattened_enums_keep_both_parts() {
        // Iptables: State (internal tag) + Action, and the flattened
        // Table/Details enum, all in one object
        let schema = schema();
        let states = schema
            .alternatives(&schema.root()["$defs"]["IptablesExpectedState"])
            .unwrap();
        let labels: Vec<&str> = states.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["Present", "Absent"]);
        // Present, blank: its tag, its own fields; the table not chosen yet
        let present = schema.blank_for(&states[0].schema);
        assert_eq!(present["State"], "Present");
        assert!(present.get("Action").is_some(), "{present}");
        assert!(present.get("Table").is_none(), "{present}");
        // the table's choices, next to the State's fields
        let rule = &schema.resolve(&states[0].schema)["oneOf"];
        let tables = schema.alternatives(&json!({ "oneOf": rule })).unwrap();
        assert!(tables.iter().any(|t| t.label == "Filter"), "{tables:?}");
    }

    #[test]
    fn missing_lists_what_to_fill_in() {
        let schema = schema();
        let root = schema.root().clone();
        let fresh = schema.blank_for(&root);
        assert_eq!(schema.missing(&root, &fresh), ["Privilege", "Nature"]);
        let started = json!({"Privilege": "None",
            "Detail": {"LineInFile": {"FilePath": "", "State": null, "Line": null}}});
        assert_eq!(
            schema.missing(&root, &started),
            [
                "Nature › LineInFile › FilePath",
                "Nature › LineInFile › State",
                "Nature › LineInFile › Line"
            ]
        );
        let done = json!({"Privilege": "None", "Detail": {"LineInFile": {"FilePath": "/etc/motd",
            "State": {"Present": {"Position": {"InsertAfter": "EOF"}}}, "Line": {"Raw": "hello"}}}});
        assert!(
            schema.missing(&root, &done).is_empty(),
            "{:?}",
            schema.missing(&root, &done)
        );
        // an empty text chosen in a picker (Line › Raw › text) is still to fill in
        let mut empty_line = done.clone();
        empty_line["Detail"]["LineInFile"]["Line"] = json!({"Raw": ""});
        assert_eq!(
            schema.missing(&root, &empty_line),
            ["Nature › LineInFile › Line › Raw"]
        );
    }

    #[test]
    fn every_nature_has_its_category() {
        let schema = schema();
        let natures = schema
            .alternatives(&schema.root()["properties"]["Detail"])
            .unwrap();
        let uncategorized: Vec<&str> = natures
            .iter()
            .map(|n| n.label.as_str())
            .filter(|n| category_of(n) == "other")
            .collect();
        assert!(
            uncategorized.is_empty(),
            "add to CATEGORIES: {uncategorized:?}"
        );
        assert_eq!(category_of("Apt"), "package");
        assert_eq!(category_of("Ollama"), "ai");
    }

    #[test]
    fn unset_options_are_absent() {
        let loaded =
            strip_nulls(json!([{"Name": "x", "Timeout": null, "Detail": {"A": {"B": null}}}]));
        assert_eq!(loaded, json!([{"Name": "x", "Detail": {"A": {}}}]));
    }
}
