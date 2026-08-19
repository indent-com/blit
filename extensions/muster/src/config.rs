//! Unit files, stacks, instances, and the substitution that turns one stack
//! into many.
//!
//! Substitution runs over `serde_json::Value` rather than over the file text,
//! which is what makes "string values, never keys" true by construction rather
//! than by a careful regex.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// A parse or validation failure, addressed well enough to fix.
#[derive(Debug, Clone)]
pub struct ConfigError {
    pub file: String,
    pub detail: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.file, self.detail)
    }
}

impl ConfigError {
    pub fn new(file: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            detail: detail.into(),
        }
    }
}

/// Milliseconds, spelled `"30s"` or `30000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration(pub u64);

impl Duration {
    pub const fn ms(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        match Value::deserialize(d)? {
            Value::Number(n) => n
                .as_u64()
                .map(Duration)
                .ok_or_else(|| D::Error::custom("duration must be a non-negative number of ms")),
            Value::String(s) => parse_duration(&s).map(Duration).ok_or_else(|| {
                D::Error::custom(format!(
                    "{s:?} is not a duration: use 250ms, 30s, 5m, 2h, or a number of ms"
                ))
            }),
            _ => Err(D::Error::custom("duration must be a string or a number")),
        }
    }
}

fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, mult) = if let Some(rest) = text.strip_suffix("ms") {
        (rest, 1)
    } else if let Some(rest) = text.strip_suffix('s') {
        (rest, 1000)
    } else if let Some(rest) = text.strip_suffix('m') {
        (rest, 60_000)
    } else if let Some(rest) = text.strip_suffix('h') {
        (rest, 3_600_000)
    } else {
        (text, 1)
    };
    digits.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// How a unit proves it is up.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReadyWhen {
    /// The create succeeded — the program resolved and `execve` ran.
    #[default]
    Spawn,
    /// Someone calls `@muster ready`.
    Manual,
    Delay(Duration),
    /// A path exists.
    Path(String),
    /// A substring appears in the terminal after the unit started.
    Log(String),
    /// A TCP connect succeeds.
    Tcp(String),
    /// A GET answers below 500.
    Http(String),
}

impl ReadyWhen {
    /// Whether this probe asks the world a question whose answer is about
    /// *now*, and can therefore be re-run against a terminal someone else
    /// started.
    ///
    /// `log`, `delay` and `spawn` describe a past event instead, and
    /// re-checking one after adoption is not possible: the evidence is a line
    /// that may have been evicted, or a moment that has passed. Re-running one
    /// stalls a healthy unit until `timeoutStart` and then replaces it — the
    /// restart storm adoption exists to prevent.
    pub const fn is_stateless(&self) -> bool {
        matches!(self, Self::Path(_) | Self::Tcp(_) | Self::Http(_))
    }
}

impl<'de> Deserialize<'de> for ReadyWhen {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        match Value::deserialize(d)? {
            Value::String(s) if s == "spawn" => Ok(Self::Spawn),
            Value::String(s) if s == "manual" => Ok(Self::Manual),
            Value::String(s) => Err(D::Error::custom(format!(
                "{s:?} is not a readyWhen: use \"spawn\", \"manual\", or one of \
                 {{delay|path|log|tcp|http}}"
            ))),
            Value::Object(map) => {
                let mut it = map.into_iter();
                let (key, value) = it
                    .next()
                    .ok_or_else(|| D::Error::custom("readyWhen object is empty"))?;
                if it.next().is_some() {
                    return Err(D::Error::custom("readyWhen takes exactly one key"));
                }
                let text = || match &value {
                    Value::String(s) => Ok(s.clone()),
                    _ => Err(D::Error::custom(format!("readyWhen {key} takes a string"))),
                };
                match key.as_str() {
                    "delay" => serde_json::from_value::<Duration>(value)
                        .map(Self::Delay)
                        .map_err(D::Error::custom),
                    "path" => text().map(Self::Path),
                    "log" => text().map(Self::Log),
                    "tcp" => text().map(Self::Tcp),
                    "http" => text().map(Self::Http),
                    other => Err(D::Error::custom(format!(
                        "readyWhen {other:?} is not one of delay, path, log, tcp, http"
                    ))),
                }
            }
            _ => Err(D::Error::custom("readyWhen must be a string or an object")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UnitType {
    #[default]
    Simple,
    Oneshot,
}

impl UnitType {
    pub const fn as_str(self) -> &'static str {
        match self {
            UnitType::Simple => "simple",
            UnitType::Oneshot => "oneshot",
        }
    }
}

/// One `envFile` entry: a path, or a path that may be absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileRef {
    pub path: String,
    pub optional: bool,
}

impl<'de> Deserialize<'de> for EnvFileRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        struct Long {
            path: String,
            #[serde(default)]
            optional: bool,
        }
        match Value::deserialize(d)? {
            Value::String(path) => Ok(Self {
                path,
                optional: false,
            }),
            other => {
                let long: Long = serde_json::from_value(other).map_err(D::Error::custom)?;
                Ok(Self {
                    path: long.path,
                    optional: long.optional,
                })
            }
        }
    }
}

/// `envFile` accepts one entry or a list; both land here.
fn deserialize_env_files<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<EnvFileRef>, D::Error> {
    match Value::deserialize(d)? {
        Value::Array(items) => items
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))
            .collect(),
        other => serde_json::from_value(other)
            .map(|one| vec![one])
            .map_err(serde::de::Error::custom),
    }
}

/// A unit file, or a stack template, as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitFile {
    pub description: Option<String>,
    #[serde(default = "yes")]
    pub autostart: bool,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub wants: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
    pub command: Option<Vec<String>>,
    pub shell: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_env_files")]
    pub env_file: Vec<EnvFileRef>,
    #[serde(rename = "type", default)]
    pub unit_type: UnitType,
    #[serde(default)]
    pub ready_when: ReadyWhen,
    #[serde(default = "yes")]
    pub restart_on_failure: bool,
    #[serde(default)]
    pub restart_on_success: bool,
    #[serde(default = "yes")]
    pub restart_on_change: bool,
    pub restart_delay: Option<Duration>,
    #[serde(default = "one")]
    pub keep: u32,
    #[serde(default = "timeout_start_default")]
    pub timeout_start: Duration,
    #[serde(default = "stop_signal_default")]
    pub stop_signal: String,
    #[serde(default = "timeout_stop_default")]
    pub timeout_stop: Duration,
    #[serde(default)]
    pub start_limit: u32,
    /// Keys muster does not know. Warned about by `doctor`, never fatal: the
    /// editor's schema is the fast path for a typo, and a newer muster may
    /// understand the key.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

const fn yes() -> bool {
    true
}
const fn one() -> u32 {
    1
}
fn timeout_start_default() -> Duration {
    Duration(30_000)
}
fn timeout_stop_default() -> Duration {
    Duration(10_000)
}
fn stop_signal_default() -> String {
    String::from("SIGTERM")
}

impl UnitFile {
    /// Keys that are deliberately meaningless: `$schema` drives the editor,
    /// `//` is where prose goes because JSON has no comments.
    pub fn unknown_keys(&self) -> Vec<&str> {
        self.extra
            .keys()
            .map(String::as_str)
            .filter(|k| *k != "$schema" && *k != "//")
            .collect()
    }

    pub fn validate(&self, file: &str) -> Result<(), ConfigError> {
        match (&self.command, &self.shell) {
            (Some(_), Some(_)) => Err(ConfigError::new(
                file,
                "command and shell are mutually exclusive",
            )),
            (None, None) => Err(ConfigError::new(file, "needs a command or a shell")),
            (Some(argv), None) if argv.is_empty() => {
                Err(ConfigError::new(file, "command is empty"))
            }
            _ => Ok(()),
        }?;
        if self.unit_type == UnitType::Oneshot && self.ready_when != ReadyWhen::Spawn {
            return Err(ConfigError::new(
                file,
                "readyWhen does not apply to a oneshot: it is ready when it exits 0",
            ));
        }
        if let Some(cwd) = &self.cwd
            && !cwd.starts_with('/')
            && !cwd.starts_with('~')
        {
            return Err(ConfigError::new(
                file,
                format!("cwd {cwd:?} must be absolute or ~-prefixed"),
            ));
        }
        Ok(())
    }
}

/// A stack's declared parameter.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VarDecl {
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    /// `"ports"` marks a base port, which is what `auto` allocates and what
    /// `doctor` checks for overlap.
    pub kind: Option<String>,
    #[serde(default = "one")]
    pub span: u32,
}

impl VarDecl {
    pub fn is_ports(&self) -> bool {
        self.kind.as_deref() == Some("ports")
    }
}

/// `stack.json` — the parameter declarations, not a unit.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StackFile {
    pub description: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, VarDecl>,
}

/// A top-level file naming a stack.
///
/// `stack` is a subdirectory name, or a path to a directory anywhere — see
/// [`is_path`]. The path form is how a stack lives in the repository it starts,
/// with only this pointer in the configuration directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceFile {
    pub stack: String,
    #[serde(default)]
    pub vars: BTreeMap<String, Value>,
    #[serde(default)]
    pub omit: Vec<String>,
    #[serde(default = "yes")]
    pub autostart: bool,
}

/// A top-level file naming a directory of ordinary units.
///
/// Unlike an instance, an include adds no suffix: the units keep their own
/// names, as though the file had been dropped in the configuration directory.
/// That is the point — a shared directory of units is not N copies of one
/// template — and it is also why two includes offering the same name is an
/// error rather than a merge.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncludeFile {
    pub include: String,
    #[serde(default)]
    pub omit: Vec<String>,
    #[serde(default = "yes")]
    pub autostart: bool,
}

/// The name a file at the top level claims, and what kind of thing it is.
pub enum TopLevel {
    Unit(Box<UnitFile>),
    Instance(Box<InstanceFile>),
    Include(Box<IncludeFile>),
}

/// Whether a `stack`/`include` value points outside the configuration
/// directory.
///
/// A bare word is a subdirectory; anything with a separator or a `~` is a path.
/// There is no third syntax to remember, and a subdirectory name containing a
/// slash was never meaningful.
pub fn is_path(value: &str) -> bool {
    value.starts_with('~') || value.starts_with('/') || value.contains('/')
}

/// A top-level file is a unit unless it has a `stack` or an `include`.
pub fn parse_top_level(file: &str, bytes: &[u8]) -> Result<TopLevel, ConfigError> {
    let value = parse_json(file, bytes)?;
    let named = |field: &str| value.get(field).is_some();
    match (named("stack"), named("include")) {
        (true, true) => Err(ConfigError::new(
            file,
            "stack and include are mutually exclusive: one instantiates a \
             template, the other adopts units as they are",
        )),
        (true, false) => serde_json::from_value(value)
            .map(|i| TopLevel::Instance(Box::new(i)))
            .map_err(|e| ConfigError::new(file, e.to_string())),
        (false, true) => serde_json::from_value(value)
            .map(|i| TopLevel::Include(Box::new(i)))
            .map_err(|e| ConfigError::new(file, e.to_string())),
        (false, false) => {
            let unit: UnitFile =
                serde_json::from_value(value).map_err(|e| ConfigError::new(file, e.to_string()))?;
            unit.validate(file)?;
            Ok(TopLevel::Unit(Box::new(unit)))
        }
    }
}

pub fn parse_json(file: &str, bytes: &[u8]) -> Result<Value, ConfigError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| ConfigError::new(file, "not UTF-8".to_string()))?;
    serde_json::from_str(text).map_err(|e| ConfigError::new(file, e.to_string()))
}

/// Substitute `${NAME}`, `${NAME+N}` and `${NAME-N}` through every string in a
/// template.
///
/// `${` is the only trigger, so a bare `$` is literal and a `shell` template
/// can still write `$BLIT_DEV_SOCK` and mean the shell's variable.
pub fn substitute(value: &mut Value, vars: &BTreeMap<String, Value>) -> Result<(), String> {
    match value {
        Value::String(s) => {
            if let Some(replaced) = substitute_str(s, vars)? {
                *s = replaced;
            }
            Ok(())
        }
        Value::Array(items) => items.iter_mut().try_for_each(|v| substitute(v, vars)),
        // Keys are deliberately untouched.
        Value::Object(map) => map.values_mut().try_for_each(|v| substitute(v, vars)),
        _ => Ok(()),
    }
}

/// `None` when the string held no `${`, so the common case allocates nothing.
fn substitute_str(text: &str, vars: &BTreeMap<String, Value>) -> Result<Option<String>, String> {
    if !text.contains("${") {
        return Ok(None);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unclosed ${{ in {text:?}"))?;
        out.push_str(&resolve(&after[..end], vars)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(Some(out))
}

/// One `${...}` body: a name, optionally `+N` or `-N`.
fn resolve(body: &str, vars: &BTreeMap<String, Value>) -> Result<String, String> {
    let (name, offset) = match body.find(['+', '-']) {
        Some(at) => {
            let digits = &body[at + 1..];
            let magnitude: i64 = digits
                .trim()
                .parse()
                .map_err(|_| format!("${{{body}}}: {digits:?} is not an integer offset"))?;
            let signed = if body.as_bytes()[at] == b'-' {
                -magnitude
            } else {
                magnitude
            };
            (body[..at].trim(), Some(signed))
        }
        None => (body.trim(), None),
    };
    let value = vars
        .get(name)
        .ok_or_else(|| format!("${{{body}}}: no parameter named {name:?}"))?;
    match offset {
        None => Ok(match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
        Some(offset) => {
            let base = as_integer(value)
                .ok_or_else(|| format!("${{{body}}}: {name} is not an integer"))?;
            Ok((base + offset).to_string())
        }
    }
}

fn as_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Bind an instance's `vars` against a stack's declarations.
///
/// Undeclared and missing-required both fail the instance rather than
/// defaulting: a parameter you forgot to bind should not silently produce
/// `http://127.0.0.1:/`.
pub fn bind_vars(
    instance: &str,
    stack_name: &str,
    stack_dir: &str,
    stack: &StackFile,
    supplied: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, String> {
    let mut bound = BTreeMap::new();
    for (name, value) in supplied {
        if !stack.vars.contains_key(name) {
            return Err(format!(
                "{name:?} is not a parameter of stack {stack_name:?}"
            ));
        }
        bound.insert(name.clone(), value.clone());
    }
    for (name, decl) in &stack.vars {
        if decl.required && !bound.contains_key(name) {
            return Err(format!("stack {stack_name:?} requires {name:?}"));
        }
    }
    bound.insert("INSTANCE".into(), Value::String(instance.to_string()));
    bound.insert("STACK".into(), Value::String(stack_name.to_string()));
    // Absolute, so a stack that lives in the repository it starts can locate
    // that repository without the instance restating a path it already knows.
    bound.insert("STACK_DIR".into(), Value::String(stack_dir.to_string()));
    Ok(bound)
}

/// The port block an instance occupies, for overlap detection.
pub fn port_span(stack: &StackFile, vars: &BTreeMap<String, Value>) -> Option<(i64, u32)> {
    let (name, decl) = stack.vars.iter().find(|(_, d)| d.is_ports())?;
    let base = as_integer(vars.get(name)?)?;
    Some((base, decl.span.max(1)))
}

/// The lowest block of `span` starting at or above `seed` that overlaps none
/// of `taken`.
///
/// `seed` is where this stack's existing instances start, which is why `auto`
/// needs one: it means "another one of these", not "some free ports". Nothing
/// in a stack declaration says which range is the machine's to use, and
/// inventing one would collide with whatever already lives there.
///
/// `taken` is every instance's block, not just this stack's — the collision
/// that matters is with anything, and two stacks that happened to be seeded
/// nearby are exactly the case `doctor` was written for.
pub fn next_port_block(taken: &[(i64, u32)], seed: Option<i64>, span: u32) -> Option<i64> {
    let span = i64::from(span.max(1));
    let mut base = seed?;
    while taken.iter().any(|(other, other_span)| {
        base < other + i64::from((*other_span).max(1)) && *other < base + span
    }) {
        base += span;
    }
    Some(base)
}

/// A `NAME=VALUE` right-hand side, typed the way a JSON file would type it.
///
/// `PORTS=10000` has to become a number, because [`port_span`] reads an integer
/// and a quoted one would not be a port block. Otherwise the value is the text
/// you typed: that is what makes `ROOT=/src/blit` work unquoted, and what keeps
/// `who=world` from being a parse error rather than a name. Nothing is
/// unwrapped — the shell already decided what reached us — and structure on a
/// command line is somebody escaping their way into a mistake, since a stack
/// parameter is a scalar.
pub fn scalar(value: &str) -> Value {
    match serde_json::from_str::<Value>(value) {
        Ok(parsed) if parsed.is_number() || parsed.is_boolean() => parsed,
        _ => Value::String(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn durations_accept_suffixes_and_bare_milliseconds() {
        assert_eq!(parse_duration("250ms"), Some(250));
        assert_eq!(parse_duration("30s"), Some(30_000));
        assert_eq!(parse_duration("5m"), Some(300_000));
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("400"), Some(400));
        assert_eq!(parse_duration("soon"), None);
    }

    #[test]
    fn substitution_offsets_a_port_block() {
        let v = vars(&[("PORTS", Value::from(10010))]);
        assert_eq!(
            substitute_str("127.0.0.1:${PORTS+1}", &v).unwrap().unwrap(),
            "127.0.0.1:10011"
        );
        assert_eq!(substitute_str("${PORTS-10}", &v).unwrap().unwrap(), "10000");
    }

    #[test]
    fn a_bare_dollar_is_left_for_the_shell() {
        let v = vars(&[("INSTANCE", Value::from("epic"))]);
        // The shell's variable survives; ours is replaced.
        assert_eq!(
            substitute_str("rm -f $BLIT_DEV_SOCK && echo ${INSTANCE}", &v)
                .unwrap()
                .unwrap(),
            "rm -f $BLIT_DEV_SOCK && echo epic"
        );
        assert_eq!(substitute_str("no markers here", &v).unwrap(), None);
    }

    #[test]
    fn an_unbound_parameter_fails_rather_than_emptying() {
        let v = vars(&[("PORTS", Value::from(1))]);
        let err = substitute_str("http://127.0.0.1:${PORT}/", &v).unwrap_err();
        assert!(err.contains("PORT"), "{err}");
        assert!(
            substitute_str("${PORTS", &v)
                .unwrap_err()
                .contains("unclosed")
        );
    }

    #[test]
    fn substitution_never_touches_keys() {
        let mut value: Value = serde_json::from_str(r#"{"${INSTANCE}":"${INSTANCE}"}"#).unwrap();
        substitute(&mut value, &vars(&[("INSTANCE", Value::from("epic"))])).unwrap();
        assert_eq!(value.to_string(), r#"{"${INSTANCE}":"epic"}"#);
    }

    #[test]
    fn a_unit_needs_exactly_one_of_command_and_shell() {
        let both = r#"{"command":["a"],"shell":"a"}"#;
        let unit: UnitFile = serde_json::from_str(both).unwrap();
        assert!(unit.validate("x.json").is_err());
        let neither: UnitFile = serde_json::from_str("{}").unwrap();
        assert!(neither.validate("x.json").is_err());
        let one: UnitFile = serde_json::from_str(r#"{"command":["a"]}"#).unwrap();
        assert!(one.validate("x.json").is_ok());
    }

    #[test]
    fn defaults_match_the_rfc() {
        let unit: UnitFile = serde_json::from_str(r#"{"command":["a"]}"#).unwrap();
        assert!(unit.autostart);
        assert!(unit.restart_on_failure);
        assert!(!unit.restart_on_success);
        assert!(unit.restart_on_change);
        assert_eq!(unit.keep, 1);
        assert_eq!(unit.timeout_start.ms(), 30_000);
        assert_eq!(unit.timeout_stop.ms(), 10_000);
        assert_eq!(unit.start_limit, 0);
    }

    #[test]
    fn schema_and_comment_keys_are_not_unknown() {
        let unit: UnitFile =
            serde_json::from_str(r#"{"command":["a"],"$schema":"../s.json","//":"prose","wat":1}"#)
                .unwrap();
        assert_eq!(unit.unknown_keys(), vec!["wat"]);
    }

    #[test]
    fn a_stack_field_makes_it_an_instance() {
        let unit = parse_top_level("a.json", br#"{"command":["a"]}"#).unwrap();
        assert!(matches!(unit, TopLevel::Unit(_)));
        let inst = parse_top_level("b.json", br#"{"stack":"blit","vars":{}}"#).unwrap();
        assert!(matches!(inst, TopLevel::Instance(_)));
    }

    #[test]
    fn ready_when_parses_every_form() {
        let cases = [
            (r#""spawn""#, ReadyWhen::Spawn),
            (r#""manual""#, ReadyWhen::Manual),
            (r#"{"delay":"2s"}"#, ReadyWhen::Delay(Duration(2000))),
            (r#"{"path":"/tmp/s"}"#, ReadyWhen::Path("/tmp/s".into())),
            (r#"{"log":"up"}"#, ReadyWhen::Log("up".into())),
            (
                r#"{"tcp":"127.0.0.1:1"}"#,
                ReadyWhen::Tcp("127.0.0.1:1".into()),
            ),
        ];
        for (text, want) in cases {
            let got: ReadyWhen = serde_json::from_str(text).unwrap();
            assert_eq!(got, want, "{text}");
        }
        assert!(serde_json::from_str::<ReadyWhen>(r#"{"nope":1}"#).is_err());
        assert!(serde_json::from_str::<ReadyWhen>(r#"{"tcp":"a","log":"b"}"#).is_err());
    }

    #[test]
    fn env_file_accepts_one_or_many_and_marks_optional() {
        let unit: UnitFile = serde_json::from_str(
            r#"{"command":["a"],"envFile":[".env",{"path":".env.local","optional":true}]}"#,
        )
        .unwrap();
        assert_eq!(unit.env_file.len(), 2);
        assert!(!unit.env_file[0].optional);
        assert!(unit.env_file[1].optional);
        let single: UnitFile =
            serde_json::from_str(r#"{"command":["a"],"envFile":".env"}"#).unwrap();
        assert_eq!(single.env_file.len(), 1);
    }

    #[test]
    fn binding_rejects_undeclared_and_missing_parameters() {
        let stack: StackFile = serde_json::from_str(
            r#"{"vars":{"ROOT":{"required":true},"PORTS":{"kind":"ports","span":4}}}"#,
        )
        .unwrap();
        let ok = bind_vars(
            "epic",
            "blit",
            "/cfg/blit",
            &stack,
            &vars(&[("ROOT", Value::from("/src")), ("PORTS", Value::from(10010))]),
        )
        .unwrap();
        assert_eq!(ok["INSTANCE"], Value::from("epic"));
        assert_eq!(port_span(&stack, &ok), Some((10010, 4)));

        assert!(bind_vars("epic", "blit", "/cfg/blit", &stack, &vars(&[])).is_err());
        assert!(
            bind_vars(
                "epic",
                "blit",
                "/cfg/blit",
                &stack,
                &vars(&[("ROOT", Value::from("/s")), ("NOPE", Value::from(1))])
            )
            .is_err()
        );
    }

    #[test]
    fn auto_ports_fill_the_first_gap_above_the_stacks_own_seed() {
        // Two instances at 10000 and 10008 with a span of 4 leave 10004.
        assert_eq!(
            next_port_block(&[(10000, 4), (10008, 4)], Some(10000), 4),
            Some(10004)
        );
        // Packed blocks push the next one past the end.
        assert_eq!(
            next_port_block(&[(10000, 4), (10004, 4)], Some(10000), 4),
            Some(10008)
        );
        // Another stack's block is an obstacle like any other: what matters is
        // that the ports are free, not whose they would have been.
        assert_eq!(
            next_port_block(&[(10000, 4), (10004, 16)], Some(10000), 4),
            Some(10020)
        );
        // A stack with no instance yet has nothing to allocate from, and
        // guessing a range would collide with whatever already lives there.
        assert_eq!(next_port_block(&[(10000, 4)], None, 4), None);
        // A declaration that forgot its span still occupies one port.
        assert_eq!(next_port_block(&[(10000, 0)], Some(10000), 0), Some(10001));
    }

    #[test]
    fn an_assignment_is_typed_the_way_a_json_file_would_type_it() {
        assert_eq!(scalar("10000"), Value::from(10000));
        assert_eq!(scalar("true"), Value::from(true));
        // Unquoted paths and words are the common case, and both are strings.
        assert_eq!(scalar("/src/blit"), Value::from("/src/blit"));
        assert_eq!(scalar("auto"), Value::from("auto"));
        // The rule is "what you typed, unless it is a number or a boolean", so
        // a JSON string literal keeps its quotes rather than being unwrapped —
        // the shell already decided what reached us, and a second layer of
        // unquoting would make `ROOT="a b"` mean something else than `ROOT=a b`.
        assert_eq!(scalar("\"10000\""), Value::from("\"10000\""));
        // Structure on a command line is somebody escaping their way into a
        // mistake; a parameter is a scalar.
        assert_eq!(scalar("[1,2]"), Value::from("[1,2]"));
    }
}

#[cfg(test)]
mod outside_tests {
    use super::*;

    #[test]
    fn a_bare_word_is_a_subdirectory_and_anything_else_is_a_path() {
        assert!(!is_path("blit"));
        assert!(is_path("/src/blit/.blit/muster"));
        assert!(is_path("~/work/stacks/web"));
        assert!(is_path("stacks/web"));
    }

    #[test]
    fn an_include_is_not_an_instance_and_both_at_once_is_refused() {
        let include = parse_top_level("work.json", br#"{"include":"~/work/units"}"#).unwrap();
        assert!(matches!(include, TopLevel::Include(_)));
        let both = parse_top_level("x.json", br#"{"stack":"a","include":"/b"}"#);
        assert!(both.is_err());
    }

    #[test]
    fn an_external_stack_binds_stack_dir() {
        let stack = StackFile::default();
        let bound = bind_vars(
            "epic",
            "/src/blit/.blit/muster",
            "/src/blit/.blit/muster",
            &stack,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(bound["STACK_DIR"], Value::from("/src/blit/.blit/muster"));
        // A template can therefore reach its own checkout without the instance
        // restating a path it already named.
        let mut value: Value = serde_json::from_str(r#"{"cwd":"${STACK_DIR}/../.."}"#).unwrap();
        substitute(&mut value, &bound).unwrap();
        assert_eq!(value["cwd"], Value::from("/src/blit/.blit/muster/../.."));
    }
}
