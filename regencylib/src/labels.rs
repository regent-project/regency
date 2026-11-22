//! Labels and label selectors (D26). An inventory is the set of hosts whose
//! labels match its selector, written like a Loki stream selector:
//! `{env="prod", role=~"web|api", tier!="db"}`; `{}` selects every host.
//!
//! Every host has the built-in labels `host` (its id) and `source` (the
//! sources defining it, possibly several), next to its own labels.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Built-in label: the host id.
pub const HOST: &str = "host";
/// Built-in label: the sources defining the host.
pub const SOURCE: &str = "source";
/// Keys a user label cannot use.
pub const RESERVED: [&str; 2] = [HOST, SOURCE];

/// `[a-zA-Z_][a-zA-Z0-9_]*` (as in Loki/Prometheus).
pub fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Checks of a host's own labels: valid keys, no built-in key.
pub fn check_user_labels(labels: &BTreeMap<String, String>) -> Result<(), String> {
    for key in labels.keys() {
        if !valid_key(key) {
            return Err(format!(
                "label {key:?}: keys are letters, digits and '_', not starting with a digit"
            ));
        }
        if RESERVED.contains(&key.as_str()) {
            return Err(format!("label {key:?} is built in (set by regency)"));
        }
    }
    Ok(())
}

/// A host's labels for matching: its own labels plus the built-in ones.
/// A key may have several values (`source`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostLabels(BTreeMap<String, BTreeSet<String>>);

impl HostLabels {
    /// `labels`: the host's own labels (several sources: merged);
    /// `sources`: the sources defining it.
    pub fn new<'a>(
        host_id: &str,
        sources: impl IntoIterator<Item = &'a str>,
        labels: &BTreeMap<String, String>,
    ) -> Self {
        let mut map: BTreeMap<String, BTreeSet<String>> = labels
            .iter()
            .filter(|(k, _)| !RESERVED.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), BTreeSet::from([v.clone()])))
            .collect();
        map.insert(HOST.to_string(), BTreeSet::from([host_id.to_string()]));
        map.insert(
            SOURCE.to_string(),
            sources.into_iter().map(str::to_string).collect(),
        );
        Self(map)
    }

    /// Values of `key`; a missing label reads as `""` (Loki).
    fn values(&self, key: &str) -> Vec<&str> {
        match self.0.get(key) {
            Some(values) if !values.is_empty() => values.iter().map(String::as_str).collect(),
            _ => vec![""],
        }
    }

    /// The values of `key`, if the host has it.
    pub fn values_of(&self, key: &str) -> Option<&BTreeSet<String>> {
        self.0.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &BTreeSet<String>)> {
        self.0.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Re,
    Nre,
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Ne => "!=",
            Op::Re => "=~",
            Op::Nre => "!~",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Matcher {
    pub key: String,
    pub op: Op,
    pub value: String,
    /// `^(?:value)$` for `=~` / `!~`.
    regex: Option<Regex>,
}

impl PartialEq for Matcher {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.op == other.op && self.value == other.value
    }
}
impl Eq for Matcher {}

impl Matcher {
    pub fn new(key: &str, op: Op, value: &str) -> Result<Self, String> {
        if !valid_key(key) {
            return Err(format!("label key {key:?}: letters, digits and '_'"));
        }
        let regex = match op {
            Op::Re | Op::Nre => Some(
                Regex::new(&format!("^(?:{value})$"))
                    .map_err(|e| format!("{key}{}\"{value}\": {e}", op.text()))?,
            ),
            Op::Eq | Op::Ne => None,
        };
        Ok(Self {
            key: key.to_string(),
            op,
            value: value.to_string(),
            regex,
        })
    }

    fn holds_for(&self, value: &str) -> bool {
        match (&self.regex, self.op) {
            (Some(re), _) => re.is_match(value),
            (None, _) => value == self.value,
        }
    }

    pub fn matches(&self, labels: &HostLabels) -> bool {
        let values = labels.values(&self.key);
        match self.op {
            Op::Eq | Op::Re => values.iter().any(|v| self.holds_for(v)),
            Op::Ne | Op::Nre => !values.iter().any(|v| self.holds_for(v)),
        }
    }
}

impl fmt::Display for Matcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let escaped = self.value.replace('\\', "\\\\").replace('"', "\\\"");
        write!(f, "{}{}\"{escaped}\"", self.key, self.op.text())
    }
}

/// A combination of matchers, all of which must hold (D26).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelSelector {
    pub matchers: Vec<Matcher>,
}

impl LabelSelector {
    /// Every host.
    pub fn all() -> Self {
        Self::default()
    }

    /// `{key="value", ...}` from equality pairs.
    pub fn equal(pairs: &BTreeMap<String, String>) -> Result<Self, String> {
        let matchers = pairs
            .iter()
            .map(|(k, v)| Matcher::new(k, Op::Eq, v))
            .collect::<Result<_, _>>()?;
        Ok(Self { matchers })
    }

    pub fn matches(&self, labels: &HostLabels) -> bool {
        self.matchers.iter().all(|m| m.matches(labels))
    }

    /// Parse `{k="v", k!="v", k=~"re", k!~"re"}` (braces optional; values
    /// double-quoted, `\"` and `\\` escaped).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut rest = text.trim();
        if let Some(inner) = rest.strip_prefix('{') {
            rest = inner
                .strip_suffix('}')
                .ok_or("a selector opened with '{' ends with '}'")?
                .trim();
        }
        let mut matchers = Vec::new();
        let mut chars = rest.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            if chars.peek().is_none() {
                break;
            }
            let mut key = String::new();
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                key.push(chars.next().expect("peeked"));
            }
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            let op = match (chars.next(), chars.peek().copied()) {
                (Some('='), Some('~')) => {
                    chars.next();
                    Op::Re
                }
                (Some('='), _) => Op::Eq,
                (Some('!'), Some('=')) => {
                    chars.next();
                    Op::Ne
                }
                (Some('!'), Some('~')) => {
                    chars.next();
                    Op::Nre
                }
                _ => {
                    return Err(format!(
                        "after {key:?}: expected one of = != =~ !~ (e.g. env=\"prod\")"
                    ));
                }
            };
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            if chars.next() != Some('"') {
                return Err(format!("the value of {key:?} must be double-quoted"));
            }
            let mut value = String::new();
            loop {
                match chars.next() {
                    None => return Err(format!("unterminated value of {key:?}")),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c) => value.push(c),
                        None => return Err(format!("unterminated value of {key:?}")),
                    },
                    Some(c) => value.push(c),
                }
            }
            matchers.push(Matcher::new(&key, op, &value)?);
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            match chars.next() {
                None => break,
                Some(',') => {}
                Some(c) => {
                    return Err(format!(
                        "unexpected {c:?} after {key:?}: use ',' between matchers"
                    ));
                }
            }
        }
        Ok(Self { matchers })
    }
}

impl fmt::Display for LabelSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, matcher) in self.matchers.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{matcher}")?;
        }
        f.write_str("}")
    }
}

impl std::str::FromStr for LabelSelector {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        Self::parse(text)
    }
}

impl Serialize for LabelSelector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LabelSelector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, sources: &[&str], labels: &[(&str, &str)]) -> HostLabels {
        let labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        HostLabels::new(id, sources.iter().copied(), &labels)
    }

    fn sel(text: &str) -> LabelSelector {
        LabelSelector::parse(text).unwrap()
    }

    #[test]
    fn parses_and_prints_loki_selectors() {
        let s = sel(r#"{env="prod", role=~"web|api" , tier!="db",name!~"x.*"}"#);
        assert_eq!(s.matchers.len(), 4);
        assert_eq!(
            s.to_string(),
            r#"{env="prod", role=~"web|api", tier!="db", name!~"x.*"}"#
        );
        assert_eq!(sel(&s.to_string()), s);
        assert_eq!(sel("{}"), LabelSelector::all());
        assert_eq!(sel(""), LabelSelector::all());
        assert_eq!(sel(r#"env="prod""#), sel(r#"{env="prod"}"#));
        assert_eq!(sel(r#"{a="q\"uo\\te"}"#).matchers[0].value, "q\"uo\\te");
        assert_eq!(sel(r#"{a="q\"uo\\te"}"#).to_string(), r#"{a="q\"uo\\te"}"#);
        for bad in [
            r#"{env="prod""#,
            r#"{env=prod}"#,
            r#"{env:"prod"}"#,
            r#"{1env="x"}"#,
            r#"{env="x" role="y"}"#,
            r#"{role=~"("}"#,
            r#"{env="x}"#,
        ] {
            assert!(LabelSelector::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn matching_like_loki() {
        let web = host("web-01", &["git"], &[("env", "prod"), ("role", "web")]);
        let db = host("db-01", &["git", "ui"], &[("env", "prod"), ("role", "db")]);
        let bare = host("lab-1", &["ui"], &[]);
        let all = sel("{}");
        assert!(all.matches(&web) && all.matches(&bare));
        assert!(sel(r#"{env="prod"}"#).matches(&db));
        assert!(!sel(r#"{env="prod"}"#).matches(&bare));
        assert!(sel(r#"{role=~"web|api"}"#).matches(&web));
        assert!(!sel(r#"{role=~"we"}"#).matches(&web), "anchored");
        assert!(sel(r#"{role!="db"}"#).matches(&web) && sel(r#"{role!="db"}"#).matches(&bare));
        assert!(
            sel(r#"{env=""}"#).matches(&bare),
            "missing label reads as empty"
        );
        assert!(sel(r#"{host="web-01"}"#).matches(&web));
        // several sources: = holds if one matches, != if none does
        assert!(sel(r#"{source="ui"}"#).matches(&db));
        assert!(!sel(r#"{source!="ui"}"#).matches(&db));
        assert!(sel(r#"{source!~"u.*"}"#).matches(&web));
        assert!(sel(r#"{env="prod", role!~"db|cache"}"#).matches(&web));
        assert!(!sel(r#"{env="prod", role!~"db|cache"}"#).matches(&db));
    }

    #[test]
    fn user_labels_cannot_be_built_in() {
        let ok = BTreeMap::from([("env".to_string(), "prod".to_string())]);
        assert!(check_user_labels(&ok).is_ok());
        for key in ["host", "source", "1x", "a-b", ""] {
            let bad = BTreeMap::from([(key.to_string(), "v".to_string())]);
            assert!(check_user_labels(&bad).is_err(), "{key}");
        }
        // a stored reserved key never overrides the built-in value
        let labels = BTreeMap::from([("host".to_string(), "other".to_string())]);
        let h = HostLabels::new("h1", ["s"], &labels);
        assert!(sel(r#"{host="h1"}"#).matches(&h));
    }

    #[test]
    fn serde_uses_the_text_form() {
        let s = sel(r#"{env="prod"}"#);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""{env=\"prod\"}""#);
        assert_eq!(serde_json::from_str::<LabelSelector>(&json).unwrap(), s);
        assert!(serde_json::from_str::<LabelSelector>(r#""{env=prod}""#).is_err());
    }
}
