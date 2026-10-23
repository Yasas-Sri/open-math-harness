//! Runtime parameters the platform supplies.
//!
//! A configuration declares each parameter: a kind, a default, and the
//! kind's bounds. A program argument names one, `{"param": NAME}` for its
//! value or `{"each": NAME, "flag": "--x"}` for every item of a list, each
//! after `--x`. The platform's values file fills them in while the
//! configuration is loaded, before it is validated; a value the file does
//! not give, or gives wrongly, is the default. A value is one argument of a
//! program the configuration starts, never a flag: it selects no
//! executable, bind, network, grant, limit or boot entry.
//!
//! Each value is checked alone, never against another value or a literal of
//! the configuration, so a declaration's bounds carry such relations: in
//! `deploy/vm/nucleus.json`, `shell_max_timeout_ms` is 25000 to 3600000, so
//! the shell's 25-second default timeout is never cut and no command outlives
//! the one-hour `max_deadline_ms` the composition gives the kernel.
//! `the_vm_parameters_cannot_stop_the_runtime` in `tests/tests/compositions.rs`
//! checks the VM's composition with every integer at its bounds and every
//! word at each of its values.
//!
//! Whoever writes the values file (the platform, root or the runtime's user;
//! no sandbox binds `/var/lib/nucleus/secrets/settings`) can send the team's
//! model key and every prompt to another https URL through `model_base_url`,
//! the model adapter's `--base-url`, choose any model id for the model
//! parameters, and raise budgets and timeouts to their declared maxima.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::path::Path;

use nucleus_protocol::ResourcePath;
use serde::Deserialize;
use serde_json::Value;

/// Largest values file the launcher reads.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
/// Most items a list parameter may declare.
const MAX_ITEMS: usize = 16;
const MAX_URL_BYTES: usize = 512;
const HTTPS: &str = "https://";

/// A parameter's name: a lowercase letter, then lowercase letters, digits
/// and `_`, 64 at most.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub struct ParamName(String);

impl TryFrom<String> for ParamName {
    type Error = String;
    fn try_from(name: String) -> Result<Self, String> {
        if spelled(
            &name,
            64,
            |c| c.is_ascii_lowercase(),
            |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
        ) {
            Ok(Self(name))
        } else {
            Err(format!("invalid parameter name {name:?}"))
        }
    }
}

impl ParamName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ParamName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ParamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a parameter's value is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// A provider/model id: `[a-z0-9][a-z0-9._:/-]*`, at most 128 bytes,
    /// whose `/`-separated segments are a resource path of two or more (the
    /// model interface's resource path is the model id).
    Model,
    /// Distinct models, at least one and at most `max_items`.
    ModelList,
    /// `https://` and more printable ASCII, no spaces, at most 512 bytes.
    Url,
    /// `[a-z][a-z0-9_-]*`, at most 32 bytes.
    Word,
    /// A whole number from `min` to `max`, written in decimal.
    Integer,
}

/// One declared parameter.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub kind: ParamKind,
    pub default: Value,
    /// An integer's bounds, both inclusive, neither below 0.
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    /// Most items of a model list, 16 at most.
    #[serde(default)]
    pub max_items: Option<usize>,
    /// For a word: the only words the program that reads it accepts.
    #[serde(default)]
    pub one_of: Option<BTreeSet<String>>,
}

/// A program argument as the configuration writes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Arg {
    Literal(String),
    Param(ParamRef),
    Each(EachRef),
}

/// `{"param": NAME}`: the value of a parameter that is not a list.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamRef {
    pub param: ParamName,
}

/// `{"each": NAME, "flag": "--x"}`: every item of a list, each after `flag`
/// when there is one.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EachRef {
    pub each: ParamName,
    #[serde(default)]
    pub flag: Option<String>,
}

/// What the launcher reports about the platform's values file. It never
/// carries a value.
#[derive(Debug, PartialEq, Eq)]
pub enum ParameterEvent {
    /// The file exists but cannot be read or is not a values file: every
    /// parameter has its default.
    ParametersFileRefused { reason: &'static str },
    /// The file names this many parameters the configuration does not
    /// declare. They are ignored.
    ParametersUnknown { count: usize },
    /// The file's value for this parameter is not one it admits: it has its
    /// default.
    ParameterRefused { parameter: ParamName },
}

/// `text` is one character `first` admits, then ones `rest` admits, at most
/// `max` bytes in all.
fn spelled(text: &str, max: usize, first: impl Fn(char) -> bool, rest: impl Fn(char) -> bool) -> bool {
    let mut chars = text.chars();
    text.len() <= max && chars.next().is_some_and(first) && chars.all(rest)
}

fn is_word(text: &str) -> bool {
    spelled(
        text,
        32,
        |c| c.is_ascii_lowercase(),
        |c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'),
    )
}

impl ParamSpec {
    /// The fields this parameter's kind takes, and only those.
    fn check(&self) -> Result<(), &'static str> {
        let bounds = match (self.kind, self.min, self.max) {
            (ParamKind::Integer, Some(min), Some(max)) => 0 <= min && min <= max,
            (ParamKind::Integer, _, _) => false,
            (_, min, max) => min.is_none() && max.is_none(),
        };
        let items = match self.kind {
            ParamKind::ModelList => self.max_items.is_some_and(|n| (1..=MAX_ITEMS).contains(&n)),
            _ => self.max_items.is_none(),
        };
        let words = match (&self.one_of, self.kind) {
            (Some(words), ParamKind::Word) => words.iter().all(|w| is_word(w)),
            (one_of, _) => one_of.is_none(),
        };
        if bounds && items && words {
            Ok(())
        } else {
            Err(
                "an integer takes min and max (0 <= min <= max), a model list max_items (1 to 16), a word may \
                 take one_of; no kind takes another's",
            )
        }
    }

    /// `value` as program arguments, if it is one of this parameter's
    /// values.
    fn admit(&self, value: &Value) -> Option<Vec<String>> {
        let model = |value: &Value| -> Option<String> {
            let id = value.as_str()?;
            let resource = ResourcePath::new(id.split('/')).ok()?;
            let id_chars = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
            let valid =
                spelled(id, 128, id_chars, |c| id_chars(c) || matches!(c, '.' | '_' | ':' | '/' | '-'))
                    && resource.segments().len() >= 2;
            valid.then(|| id.to_owned())
        };
        let items = match self.kind {
            ParamKind::Model => vec![model(value)?],
            ParamKind::ModelList => {
                let items = value.as_array()?.iter().map(model).collect::<Option<Vec<_>>>()?;
                let distinct = items.iter().collect::<BTreeSet<_>>().len() == items.len();
                (distinct && (1..=self.max_items?).contains(&items.len())).then_some(items)?
            }
            ParamKind::Url => {
                let url = value.as_str()?;
                let valid = url.starts_with(HTTPS)
                    && (HTTPS.len() + 1..=MAX_URL_BYTES).contains(&url.len())
                    && url.bytes().all(|b| b.is_ascii_graphic());
                vec![valid.then(|| url.to_owned())?]
            }
            ParamKind::Word => {
                let word = value.as_str()?;
                let valid = is_word(word) && self.one_of.as_ref().is_none_or(|words| words.contains(word));
                vec![valid.then(|| word.to_owned())?]
            }
            ParamKind::Integer => {
                let number = value.as_i64()?;
                vec![(self.min?..=self.max?).contains(&number).then(|| number.to_string())?]
            }
        };
        // Whatever its kind, a value is an argument, never a flag.
        items.iter().all(|item| !item.starts_with('-')).then_some(items)
    }
}

/// One declared parameter with its value.
struct Declared {
    spec: ParamSpec,
    /// Its value as program arguments: the default until the platform's
    /// file gives one it admits.
    value: Vec<String>,
}

/// The declared parameters and their values.
pub(crate) struct Parameters(BTreeMap<ParamName, Declared>);

impl Parameters {
    /// Every parameter at its default. A declaration whose fields do not
    /// fit its kind, or whose default is not one of its values, is an error.
    pub(crate) fn declared(specs: &BTreeMap<ParamName, ParamSpec>) -> Result<Self, String> {
        let declare = |name: &ParamName, spec: &ParamSpec| -> Result<Declared, String> {
            spec.check().map_err(|why| format!("parameter {name}: {why}"))?;
            let value = spec
                .admit(&spec.default)
                .ok_or_else(|| format!("parameter {name}: its default is not one of its values"))?;
            Ok(Declared { spec: spec.clone(), value })
        };
        specs
            .iter()
            .map(|(name, spec)| Ok((name.clone(), declare(name, spec)?)))
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// Every argument that names a parameter names a declared one, the way
    /// its kind is named, and every declared parameter is named.
    pub(crate) fn check<'a>(&self, args: impl IntoIterator<Item = &'a Arg>) -> Result<(), String> {
        let mut named = BTreeSet::new();
        for arg in args {
            let name = match arg {
                Arg::Literal(_) => continue,
                Arg::Param(ParamRef { param }) => self.get(param, false).map(|_| param)?,
                Arg::Each(EachRef { each, .. }) => self.get(each, true).map(|_| each)?,
            };
            named.insert(name);
        }
        match self.0.keys().find(|name| !named.contains(name)) {
            Some(name) => Err(format!("parameter {name} is declared but no argument names it")),
            None => Ok(()),
        }
    }

    /// The platform's values from `file`, each one its parameter admits in
    /// place of the default. Nothing in the file is an error: what is
    /// refused goes to `report`, never with the value.
    pub(crate) fn fill(&mut self, file: &Path, report: &mut dyn FnMut(ParameterEvent)) {
        let values = match read(file) {
            Ok(Some(values)) => values,
            Ok(None) => return,
            Err(reason) => return report(ParameterEvent::ParametersFileRefused { reason }),
        };
        let unknown = values.keys().filter(|name| !self.0.contains_key(name.as_str())).count();
        if unknown > 0 {
            report(ParameterEvent::ParametersUnknown { count: unknown });
        }
        for (name, declared) in &mut self.0 {
            let Some(value) = values.get(name.as_str()) else { continue };
            match declared.spec.admit(value) {
                Some(items) => declared.value = items,
                None => report(ParameterEvent::ParameterRefused { parameter: name.clone() }),
            }
        }
    }

    /// `args` with each parameter's value in place of its name.
    pub(crate) fn expand(&self, args: Vec<Arg>) -> Result<Vec<String>, String> {
        let mut expanded = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                Arg::Literal(text) => expanded.push(text),
                Arg::Param(ParamRef { param }) => expanded.extend(self.get(&param, false)?.iter().cloned()),
                Arg::Each(EachRef { each, flag }) => {
                    for item in self.get(&each, true)? {
                        expanded.extend(flag.iter().cloned());
                        expanded.push(item.clone());
                    }
                }
            }
        }
        Ok(expanded)
    }

    /// The value of `name`, named as a list or not.
    fn get(&self, name: &ParamName, list: bool) -> Result<&[String], String> {
        let declared = self
            .0
            .get(name)
            .ok_or_else(|| format!("an argument names parameter {name}, which is not declared"))?;
        match (declared.spec.kind == ParamKind::ModelList, list) {
            (true, false) => Err(format!("parameter {name} is a list: name it with each")),
            (false, true) => Err(format!("parameter {name} is not a list: name it with param")),
            _ => Ok(&declared.value),
        }
    }
}

/// The values file: `{"v": 1, "values": {NAME: VALUE}}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValuesFile {
    v: u32,
    values: BTreeMap<String, Value>,
}

/// The values `file` gives: none when there is no file, why not when it
/// cannot be read or is not a values file.
fn read(file: &Path) -> Result<Option<BTreeMap<String, Value>>, &'static str> {
    let opened = match std::fs::File::open(file) {
        Ok(opened) => opened,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("unreadable"),
    };
    if !opened.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err("not a regular file");
    }
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_FILE_BYTES + 1).unwrap_or(u64::MAX);
    opened.take(limit).read_to_end(&mut bytes).map_err(|_| "unreadable")?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err("larger than 64 KiB");
    }
    match serde_json::from_slice::<ValuesFile>(&bytes) {
        Ok(ValuesFile { v: 1, values }) => Ok(Some(values)),
        _ => Err("not a version 1 values file"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use serde_json::json;

    use super::*;
    use crate::LauncherConfig;

    /// One program naming a parameter of every kind.
    fn declared() -> Value {
        json!({
            "parameters": {
                "models": {"kind": "model_list", "max_items": 2, "default": ["a/x", "a/y"]},
                "model": {"kind": "model", "default": "a/x"},
                "url": {"kind": "url", "default": "https://example.test/v1"},
                "mood": {"kind": "word", "one_of": ["brisk", "calm"], "default": "calm"},
                "calls": {"kind": "integer", "min": 1, "max": 100, "default": 10}
            },
            "programs": {"agent": {
                "executable": "bin/agent",
                "args": ["--fixed", {"each": "models", "flag": "--model"}, "--image", {"param": "model"},
                         "--url", {"param": "url"}, "--mood", {"param": "mood"}, {"each": "models"},
                         {"param": "calls"}],
                "binds": [{"host": "out", "guest": "/out", "writable": true}]
            }},
            "boot": [{"program": "agent", "run_to_completion": true}]
        })
    }

    const DEFAULTS: [&str; 14] = [
        "--fixed",
        "--model",
        "a/x",
        "--model",
        "a/y",
        "--image",
        "a/x",
        "--url",
        "https://example.test/v1",
        "--mood",
        "calm",
        "a/x",
        "a/y",
        "10",
    ];

    /// The configuration `config`, in `dir`, with the values file `values`
    /// (none when `None`), and what was reported.
    fn load(
        dir: &Path,
        config: &Value,
        values: Option<&[u8]>,
    ) -> (Result<LauncherConfig, String>, Vec<ParameterEvent>) {
        let path = dir.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let file = dir.join("parameters.json");
        let _ = std::fs::remove_file(&file);
        if let Some(values) = values {
            std::fs::write(&file, values).unwrap();
        }
        let mut events = Vec::new();
        let loaded = LauncherConfig::load_with(&path, Some(&file), |event| events.push(event));
        (loaded, events)
    }

    fn values(values: Value) -> Vec<u8> {
        json!({"v": 1, "values": values}).to_string().into_bytes()
    }

    fn args(config: &LauncherConfig) -> Vec<&str> {
        config.programs.values().flat_map(|p| &p.args).map(String::as_str).collect()
    }

    /// Everything but the arguments, comparable.
    fn authority(config: &LauncherConfig) -> String {
        let programs: Vec<_> = config
            .programs
            .iter()
            .map(|(name, p)| format!("{name} {:?} {:?} {}", p.executable, p.binds, p.network))
            .collect();
        format!(
            "{programs:?} {:?} {:?} {:?} {:?}",
            config.boot, config.limits, config.identities, config.lifetime
        )
    }

    #[test]
    fn without_a_file_every_parameter_has_its_default_and_values_change_only_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let (defaults, events) = load(dir.path(), &declared(), None);
        let defaults = defaults.unwrap();
        assert_eq!((args(&defaults), events), (DEFAULTS.to_vec(), vec![]));
        let given = values(json!({"models": ["a/z"], "model": "a/y", "url": "https://other.test",
                                  "mood": "brisk", "calls": 100}));
        let (filled, events) = load(dir.path(), &declared(), Some(&given));
        let filled = filled.unwrap();
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(
            args(&filled),
            [
                "--fixed",
                "--model",
                "a/z",
                "--image",
                "a/y",
                "--url",
                "https://other.test",
                "--mood",
                "brisk",
                "a/z",
                "100"
            ]
        );
        assert_eq!(authority(&filled), authority(&defaults));
        // Names of anything but a declared parameter are counted once and ignored.
        let foreign = values(json!({"executable": "/bin/sh", "binds": [], "grants": [], "network": true}));
        let (ignored, events) = load(dir.path(), &declared(), Some(&foreign));
        let ignored = ignored.unwrap();
        assert_eq!(events, [ParameterEvent::ParametersUnknown { count: 4 }]);
        assert_eq!((args(&ignored), authority(&ignored)), (DEFAULTS.to_vec(), authority(&defaults)));
    }

    #[test]
    fn a_value_its_parameter_does_not_admit_is_the_default_with_an_event_naming_the_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let long_url = format!("https://{}", "a".repeat(MAX_URL_BYTES));
        for (name, refused) in [
            ("calls", json!(0)),
            ("calls", json!(101)),
            ("calls", json!(-5)),
            ("calls", json!(1.5)),
            ("calls", json!("10")),
            ("mood", json!("-calm")),
            ("mood", json!("angry")),
            ("mood", json!("Calm")),
            ("model", json!("-a/x")),
            ("model", json!("a")),
            ("model", json!("a//x")),
            ("model", json!("a/../x")),
            ("model", json!(["a/x"])),
            ("models", json!([])),
            ("models", json!(["a/x", "a/x"])),
            ("models", json!(["a/x", "a/y", "a/z"])),
            ("models", json!("a/x")),
            ("url", json!("http://example.test")),
            ("url", json!("https://a b")),
            ("url", json!("https://")),
            ("url", json!(long_url)),
        ] {
            let (config, events) =
                load(dir.path(), &declared(), Some(&values(json!({ name: refused.clone() }))));
            assert_eq!(args(&config.unwrap()), DEFAULTS, "{name} {refused}");
            let parameter = ParamName::try_from(name.to_owned()).unwrap();
            assert_eq!(events, [ParameterEvent::ParameterRefused { parameter }], "{name} {refused}");
        }
    }

    #[test]
    fn a_file_that_is_not_a_values_file_gives_every_default_with_one_event() {
        let dir = tempfile::tempdir().unwrap();
        let padded = |len: usize| {
            let mut file = values(json!({"calls": 42}));
            file.resize(len, b' ');
            file
        };
        let (config, events) = load(dir.path(), &declared(), Some(&padded(MAX_FILE_BYTES)));
        assert!(events.is_empty() && args(&config.unwrap()).contains(&"42"), "64 KiB is read");
        for file in [
            padded(MAX_FILE_BYTES + 1),
            b"{".to_vec(),
            br#"{"v": 2, "values": {"calls": 42}}"#.to_vec(),
            br#"{"v": 1, "values": {"calls": 42}, "more": 1}"#.to_vec(),
            br#"{"v": 1, "values": [42]}"#.to_vec(),
        ] {
            let (config, events) = load(dir.path(), &declared(), Some(&file));
            assert_eq!(args(&config.unwrap()), DEFAULTS);
            assert!(matches!(events[..], [ParameterEvent::ParametersFileRefused { .. }]), "{events:?}");
        }
    }

    #[test]
    fn a_declaration_or_a_reference_that_is_wrong_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        type Edit = fn(&mut Value);
        let edits: [(&str, Edit); 11] = [
            ("an undeclared name", |c| c["programs"]["agent"]["args"][0] = json!({"param": "nope"})),
            ("a list as one value", |c| c["programs"]["agent"]["args"][0] = json!({"param": "models"})),
            ("each of one value", |c| c["programs"]["agent"]["args"][0] = json!({"each": "calls"})),
            ("a reference with more", |c| {
                c["programs"]["agent"]["args"][0] = json!({"param": "calls", "flag": "--x"})
            }),
            ("a parameter no argument names", |c| {
                c["parameters"]["spare"] = json!({"kind": "url",
                "default": "https://x.test"})
            }),
            ("a default out of bounds", |c| c["parameters"]["calls"]["default"] = json!(1000)),
            ("an integer without max", |c| {
                c["parameters"]["calls"] = json!({"kind": "integer", "min": 1,
                "default": 10})
            }),
            ("a negative bound", |c| c["parameters"]["calls"]["min"] = json!(-1)),
            ("a model id that is not provider/model", |c| c["parameters"]["model"]["default"] = json!("a")),
            ("a field of another kind", |c| c["parameters"]["mood"]["max_items"] = json!(2)),
            ("an unknown field", |c| c["parameters"]["calls"]["step"] = json!(1)),
        ];
        assert!(load(dir.path(), &declared(), None).0.is_ok());
        for (why, edit) in edits {
            let mut config = declared();
            edit(&mut config);
            assert!(load(dir.path(), &config, None).0.is_err(), "{why} should be rejected");
        }
    }
}
