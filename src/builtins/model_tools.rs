use crate::plugin::{ModelTool, ModelToolDescriptor, ModelToolOutput, Plugin, PluginHost};
use crate::prompts;
use crate::protocol::{ProtocolRegistry, split_address};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub(crate) const MAX_STEPS: usize = 8;
pub(crate) const MAX_OPERATIONS: usize = 64;
pub(crate) const MAX_FOR_ELEMENTS: usize = 32;

const STEP_SHAPE_EXAMPLE: &str = r#"{"read": "file://src/main.rs"}"#;

#[derive(Clone, Copy, Debug, PartialEq)]
enum ProtocolOperation {
    Read,
    Exec,
}

struct ProtocolTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolArguments {
    steps: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepArgument {
    read: Option<String>,
    exec: Option<String>,
    input: Option<Map<String, Value>>,
    id: Option<String>,
    r#if: Option<String>,
    r#for: Option<String>,
    max: Option<i64>,
    show: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpArguments {
    protocols: Vec<String>,
}

struct HelpTool;

/// One step after whole-call validation.
#[derive(Debug)]
struct ValidatedStep {
    operation: ProtocolOperation,
    address: String,
    input: Option<Map<String, Value>>,
    /// Top-level input fields this step's protocol executes verbatim; their
    /// values are never substituted.
    literal_fields: Vec<String>,
    id: Option<String>,
    condition: Option<Expr>,
    loop_spec: Option<LoopSpec>,
    max: usize,
    show: Show,
    /// Root names referenced by this step's `input`, address, and `for`
    /// source. A reference to a failed or skipped step skips this step.
    data_refs: Vec<String>,
    /// True when any later step references this step's `id`; the operation
    /// then runs pinned in the foreground instead of auto-backgrounding.
    referenced_later: bool,
}

#[derive(Debug)]
struct LoopSpec {
    variable: String,
    source: Operand,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Show {
    All,
    Errors,
    None,
}

impl Show {
    fn shows_output(self, failed: bool) -> bool {
        match self {
            Self::All => true,
            Self::Errors => failed,
            Self::None => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Operand {
    Reference {
        root: String,
        segments: Vec<Segment>,
    },
    Literal(Value),
}

#[derive(Clone, Debug, PartialEq)]
enum Segment {
    Field(String),
    Index(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Comparator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
struct Expr {
    negated: bool,
    left: Operand,
    comparison: Option<(Comparator, Operand)>,
}

impl Expr {
    /// Every reference root the expression can read.
    fn roots(&self) -> Vec<&str> {
        let right = self.comparison.as_ref().and_then(|(_, right)| right.root());
        self.left.root().into_iter().chain(right).collect()
    }
}

impl Operand {
    fn root(&self) -> Option<&str> {
        match self {
            Self::Reference { root, .. } => Some(root),
            Self::Literal(_) => None,
        }
    }

    fn eval(&self, values: &HashMap<String, Value>) -> Result<Value> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Reference { root, segments } => Ok(resolve_reference(root, segments, values)),
        }
    }
}

fn resolve_reference(root: &str, segments: &[Segment], values: &HashMap<String, Value>) -> Value {
    let mut value = values.get(root).cloned().unwrap_or(Value::Null);
    for segment in segments {
        value = match (segment, &value) {
            (Segment::Field(name), Value::Object(map)) => {
                map.get(name).cloned().unwrap_or(Value::Null)
            }
            (Segment::Index(index), Value::Array(items)) => {
                items.get(*index).cloned().unwrap_or(Value::Null)
            }
            _ => Value::Null,
        };
    }
    value
}

/// Parses the fixed expression grammar `["not"] operand [comparator operand]`.
fn parse_expression(text: &str) -> Result<Expr> {
    let mut cursor = Cursor::new(text);
    let negated = cursor.take_word("not");
    let left = cursor.parse_operand()?;
    cursor.skip_whitespace();
    let comparison = cursor.take_comparator()?;
    let expr = match comparison {
        Some(comparator) => {
            let right = cursor.parse_operand()?;
            cursor.skip_whitespace();
            if !cursor.is_empty() {
                bail!(
                    "invalid expression {:?}: unexpected text after the comparison; the \
                     grammar is `[not] operand [comparator operand]`",
                    text
                );
            }
            Expr {
                negated,
                left,
                comparison: Some((comparator, right)),
            }
        }
        None => {
            cursor.skip_whitespace();
            if !cursor.is_empty() {
                bail!(
                    "invalid expression {:?}: unexpected text after the operand; the grammar \
                     is `[not] operand [comparator operand]`",
                    text
                );
            }
            Expr {
                negated,
                left,
                comparison: None,
            }
        }
    };
    Ok(expr)
}

/// Parses one operand: a reference or a JSON literal.
fn parse_operand_str(text: &str) -> Result<Operand> {
    let mut cursor = Cursor::new(text);
    let operand = cursor.parse_operand()?;
    cursor.skip_whitespace();
    if !cursor.is_empty() {
        bail!(
            "invalid operand {:?}: a reference is `<id>` with `.field` and `[index]` segments; a \
             literal is a number, a double-quoted string, true, false, or null",
            text
        );
    }
    Ok(operand)
}

struct Cursor<'a> {
    text: &'a str,
    bytes: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text: text.trim(),
            bytes: 0,
        }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.bytes.min(self.text.len())..]
    }

    fn is_empty(&self) -> bool {
        self.rest().trim().is_empty()
    }

    fn skip_whitespace(&mut self) {
        let rest = self.rest();
        let skipped = rest.len() - rest.trim_start().len();
        self.bytes += skipped;
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Consumes `word` when it is followed by a non-identifier character.
    fn take_word(&mut self, word: &str) -> bool {
        self.skip_whitespace();
        let rest = self.rest();
        let Some(after) = rest.strip_prefix(word) else {
            return false;
        };
        if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
        self.bytes += word.len();
        true
    }

    fn take_comparator(&mut self) -> Result<Option<Comparator>> {
        self.skip_whitespace();
        let rest = self.rest();
        for (symbol, comparator) in [
            ("==", Comparator::Eq),
            ("!=", Comparator::Ne),
            ("<=", Comparator::Le),
            (">=", Comparator::Ge),
            ("<", Comparator::Lt),
            (">", Comparator::Gt),
        ] {
            if rest.starts_with(symbol) {
                self.bytes += symbol.len();
                return Ok(Some(comparator));
            }
        }
        if rest.starts_with('=') {
            bail!(
                "invalid comparator: use `==` for equality; the comparators are ==, !=, <, <=, >, \
                 and >="
            );
        }
        Ok(None)
    }

    fn parse_operand(&mut self) -> Result<Operand> {
        self.skip_whitespace();
        let rest = self.rest();
        let Some(first) = rest.chars().next() else {
            bail!(
                "invalid expression {:?}: an operand is missing; a reference is `<id>` with \
                 `.field` and `[index]` segments, or a literal number, double-quoted string, \
                 true, false, or null",
                self.text
            );
        };
        if first == '"' {
            return Ok(Operand::Literal(Value::String(
                self.parse_string_literal()?,
            )));
        }
        if first.is_ascii_digit() || first == '-' {
            return Ok(Operand::Literal(self.parse_number_literal()?));
        }
        for (keyword, literal) in [
            ("true", Value::Bool(true)),
            ("false", Value::Bool(false)),
            ("null", Value::Null),
        ] {
            if self.take_word(keyword) {
                return Ok(Operand::Literal(literal));
            }
        }
        if first.is_ascii_lowercase() {
            return self.parse_reference();
        }
        bail!(
            "invalid operand {:?}: references start with a lowercase step `id` or `for` \
             variable; literals are numbers, double-quoted strings, true, false, and null",
            self.text
        );
    }

    fn parse_string_literal(&mut self) -> Result<String> {
        let rest = self.rest();
        let mut literal = String::new();
        let mut characters = rest.char_indices();
        let (_, opening) = characters.next().expect("the caller checked the quote");
        debug_assert_eq!(opening, '"');
        while let Some((offset, character)) = characters.next() {
            match character {
                '"' => {
                    self.bytes += offset + 1;
                    return Ok(literal);
                }
                '\\' => {
                    let (_, escape) = characters.next().ok_or_else(|| {
                        anyhow!(
                            "invalid string literal in {:?}: trailing backslash",
                            self.text
                        )
                    })?;
                    literal.push(match escape {
                        '"' => '"',
                        '\\' => '\\',
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => bail!(
                            "invalid escape `\\{other}` in string literal; supported escapes are \
                             \\\", \\\\, \\n, \\t, and \\r"
                        ),
                    });
                }
                other => literal.push(other),
            }
        }
        bail!(
            "invalid string literal in {:?}: the closing quote is missing",
            self.text
        );
    }

    fn parse_number_literal(&mut self) -> Result<Value> {
        let rest = self.rest();
        let end = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
            .map(|(offset, c)| offset + c.len_utf8())
            .last()
            .unwrap_or(0);
        let text = &rest[..end];
        let value =
            serde_json::from_str(text).map_err(|_| anyhow!("invalid number literal {text:?}"))?;
        self.bytes += end;
        Ok(value)
    }

    fn parse_reference(&mut self) -> Result<Operand> {
        let rest = self.rest();
        let root_end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let root = &rest[..root_end];
        if root.is_empty() {
            bail!("invalid reference in {:?}", self.text);
        }
        self.bytes += root_end;
        let mut segments = Vec::new();
        loop {
            match self.peek() {
                Some('.') => {
                    self.bytes += 1;
                    let rest = self.rest();
                    let end = rest
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                        .unwrap_or(rest.len());
                    if end == 0 {
                        bail!(
                            "invalid reference in {:?}: a `.` must be followed by a field name",
                            self.text
                        );
                    }
                    segments.push(Segment::Field(rest[..end].to_string()));
                    self.bytes += end;
                }
                Some('[') => {
                    self.bytes += 1;
                    let rest = self.rest();
                    let Some(end) = rest.find(']') else {
                        bail!(
                            "invalid reference in {:?}: a `[` index must end with `]`",
                            self.text
                        );
                    };
                    let index = rest[..end]
                        .parse::<usize>()
                        .map_err(|_| anyhow!("invalid index {:?} in reference", &rest[..end]))?;
                    segments.push(Segment::Index(index));
                    self.bytes += end + 1;
                }
                _ => break,
            }
        }
        Ok(Operand::Reference {
            root: root.to_string(),
            segments,
        })
    }
}

fn eval_expression(expr: &Expr, values: &HashMap<String, Value>) -> Result<bool> {
    let result = match &expr.comparison {
        None => truthy(&expr.left.eval(values)?)?,
        Some((comparator, right)) => {
            compare_operands(*comparator, &expr.left.eval(values)?, &right.eval(values)?)?
        }
    };
    Ok(result != expr.negated)
}

fn truthy(value: &Value) -> Result<bool> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::Null => Ok(false),
        other => bail!(
            "a condition without a comparator must be true, false, or null; {} is not comparable \
             to nothing",
            json_type_name(other)
        ),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => left == right,
    }
}

fn compare_operands(comparator: Comparator, left: &Value, right: &Value) -> Result<bool> {
    match comparator {
        Comparator::Eq => Ok(values_equal(left, right)),
        Comparator::Ne => Ok(!values_equal(left, right)),
        Comparator::Lt | Comparator::Le | Comparator::Gt | Comparator::Ge => {
            let ordering = match (left, right) {
                (Value::Number(left), Value::Number(right)) => left
                    .as_f64()
                    .partial_cmp(&right.as_f64())
                    .ok_or_else(|| anyhow!("numbers are not comparable"))?,
                (Value::String(left), Value::String(right)) => left.cmp(right),
                _ => bail!(
                    "ordering comparisons accept two numbers or two strings; got {} and {}",
                    json_type_name(left),
                    json_type_name(right)
                ),
            };
            use std::cmp::Ordering::*;
            Ok(match comparator {
                Comparator::Lt => ordering == Less,
                Comparator::Le => ordering != Greater,
                Comparator::Gt => ordering == Greater,
                Comparator::Ge => ordering != Less,
                other => unreachable!("comparator {other:?} is handled above"),
            })
        }
    }
}

/// The operand of a string that is exactly `{{ operand }}`, with optional
/// surrounding spaces inside the braces. Any other string, including
/// template text such as `{{a}} and {{b}}`, is literal.
fn whole_placeholder(text: &str) -> Option<Operand> {
    let inner = text.strip_prefix("{{")?.strip_suffix("}}")?;
    parse_operand_str(inner).ok()
}

/// Every `{{ operand }}` span inside an address string.
fn address_placeholders(text: &str) -> Result<Vec<(usize, usize, String)>> {
    let mut spans = Vec::new();
    let mut offset = 0;
    while let Some(start) = text[offset..].find("{{") {
        let start = offset + start;
        let Some(found) = text[start + 2..].find("}}") else {
            bail!("invalid placeholder in address {text:?}: the closing `}}` is missing");
        };
        let close = start + 2 + found;
        let end = close + 2;
        spans.push((start, end, text[start + 2..close].trim().to_string()));
        offset = end;
    }
    Ok(spans)
}

fn substitute_input(
    input: &Map<String, Value>,
    values: &HashMap<String, Value>,
    literal_fields: &[String],
) -> Result<Map<String, Value>> {
    let mut substituted = Map::new();
    for (key, value) in input {
        if literal_fields.iter().any(|field| field == key) {
            substituted.insert(key.clone(), value.clone());
        } else {
            substituted.insert(key.clone(), substitute_value(value, values)?);
        }
    }
    Ok(substituted)
}

fn substitute_value(value: &Value, values: &HashMap<String, Value>) -> Result<Value> {
    match value {
        Value::String(text) => {
            if let Some(operand) = whole_placeholder(text) {
                return operand.eval(values);
            }
            Ok(value.clone())
        }
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(|item| substitute_value(item, values))
                .collect::<Result<Vec<_>>>()?,
        )),
        // Nested objects are not literal fields themselves, so substitution
        // continues through them without exemptions.
        Value::Object(map) => Ok(Value::Object(substitute_input(map, values, &[])?)),
        _ => Ok(value.clone()),
    }
}

fn substitute_address(address: &str, values: &HashMap<String, Value>) -> Result<String> {
    if address_placeholders(address)?.is_empty() {
        return Ok(address.to_string());
    }
    let mut result = String::new();
    let mut offset = 0;
    for (start, end, inner) in address_placeholders(address)? {
        result.push_str(&address[offset..start]);
        let value = parse_operand_str(&inner)?.eval(values)?;
        match &value {
            Value::String(text) => result.push_str(text),
            Value::Number(number) => result.push_str(&number.to_string()),
            Value::Bool(flag) => result.push_str(if *flag { "true" } else { "false" }),
            other => bail!(
                "address placeholders must reference a string, number, or boolean; got {}",
                json_type_name(other)
            ),
        }
        offset = end;
    }
    result.push_str(&address[offset..]);
    Ok(result)
}

#[async_trait]
impl ModelTool for ProtocolTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "protocol".to_string(),
            description: prompts::PROTOCOL_TOOL_DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/step" },
                        "minItems": 1,
                        "maxItems": MAX_STEPS,
                        "description": prompts::PROTOCOL_STEPS_DESCRIPTION
                    }
                },
                "required": ["steps"],
                "additionalProperties": false,
                "$defs": {
                    "step": {
                        "type": "object",
                        "properties": {
                            "read": {
                                "type": "string",
                                "description": "Address to read: `<protocol>://<target>`"
                            },
                            "exec": {
                                "type": "string",
                                "description": "Address to execute: `<protocol>://<target>`"
                            },
                            "input": {
                                "type": "object",
                                "description": "Protocol input; the protocol's help page defines the fields"
                            },
                            "id": {
                                "type": "string",
                                "pattern": "^[a-z][a-z0-9_]*$",
                                "description": "Name for later references, unique within the call"
                            },
                            "if": {
                                "type": "string",
                                "description": "Condition; the step runs only when it is true"
                            },
                            "for": {
                                "type": "string",
                                "description": "`<name> in <reference>`; runs the step once per list element; requires `max`"
                            },
                            "max": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": MAX_FOR_ELEMENTS,
                                "description": "Upper bound for `for`; the call fails when the list is longer"
                            },
                            "show": {
                                "type": "string",
                                "enum": ["all", "errors", "none"],
                                "description": "Whether the step's output enters the result"
                            }
                        },
                        "additionalProperties": false
                    }
                }
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: ProtocolArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| anyhow!("invalid protocol arguments: {error}"))?;
        let steps = validate_steps(&arguments.steps, protocols).await?;
        execute_steps(&steps, protocols).await
    }
}

fn step_field_error(number: usize, field: &str, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("invalid protocol arguments: step {number} field `{field}`: {message}")
}

/// Like `step_field_error`, but preserves the cause's error chain so typed
/// errors such as `ProtocolHelpRequired` stay detectable by the runtime.
fn step_field_error_from(number: usize, field: &str, error: anyhow::Error) -> anyhow::Error {
    error.context(format!(
        "invalid protocol arguments: step {number} field `{field}`"
    ))
}

fn check_reference_root(
    root: &str,
    number: usize,
    field: &str,
    earlier_ids: &HashSet<String>,
    loop_variable: Option<&str>,
) -> Result<()> {
    if earlier_ids.contains(root) {
        return Ok(());
    }
    if loop_variable == Some(root) {
        return Ok(());
    }
    let hint = match loop_variable {
        Some(variable) => format!(" or the `for` variable `{variable}`"),
        None => String::new(),
    };
    Err(step_field_error(
        number,
        field,
        format!(
            "reference `{root}` does not name an earlier step{hint}; references may point only \
             to steps declared before this one; example: {STEP_SHAPE_EXAMPLE}"
        ),
    ))
}

/// Expression keywords; an `id` or `for` variable with one of these names
/// could never be referenced.
const RESERVED_NAMES: [&str; 4] = ["not", "true", "false", "null"];

fn valid_id(id: &str) -> bool {
    let mut characters = id.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED_NAMES.contains(&id)
}

/// A JSON type name for error messages and its membership check.
type FieldType = (&'static str, fn(&Value) -> bool);

/// Expected JSON type of every step field, named in validation errors.
fn step_field_type(field: &str) -> Option<FieldType> {
    Some(match field {
        "read" | "exec" | "id" | "if" | "for" | "show" => ("a string", Value::is_string),
        "input" => ("an object", Value::is_object),
        "max" => ("an integer", |value| value.is_i64() || value.is_u64()),
        _ => return None,
    })
}

/// Rejects a step that is not an object, has an unknown field, or has a
/// field of the wrong type, naming the step and field.
fn check_step_shape(number: usize, raw_step: &Value) -> Result<()> {
    let Value::Object(object) = raw_step else {
        bail!(
            "invalid protocol arguments: step {number} must be an object, got {}; example: \
             {STEP_SHAPE_EXAMPLE}",
            json_type_name(raw_step)
        );
    };
    for (field, value) in object {
        let Some((expected, matches)) = step_field_type(field) else {
            return Err(step_field_error(
                number,
                field,
                format!(
                    "unknown step field; step fields are read, exec, input, id, if, for, max, \
                     and show; example: {STEP_SHAPE_EXAMPLE}"
                ),
            ));
        };
        if !matches(value) {
            let example = if field == "input" {
                r#"{"read": "search://src", "input": {"query": "parse config"}}"#
            } else {
                STEP_SHAPE_EXAMPLE
            };
            return Err(step_field_error(
                number,
                field,
                format!(
                    "must be {expected}, got {}; example: {example}",
                    json_type_name(value)
                ),
            ));
        }
    }
    Ok(())
}

async fn validate_steps(raw: &[Value], protocols: &ProtocolRegistry) -> Result<Vec<ValidatedStep>> {
    if raw.is_empty() {
        bail!("invalid protocol arguments: at least one step is required");
    }
    if raw.len() > MAX_STEPS {
        bail!(
            "invalid protocol arguments: at most {MAX_STEPS} steps per call, got {}",
            raw.len()
        );
    }
    let mut steps = Vec::with_capacity(raw.len());
    let mut ids = HashSet::new();
    let mut all_reference_roots = Vec::new();
    for (index, raw_step) in raw.iter().enumerate() {
        steps.push(
            validate_step(
                index + 1,
                raw_step,
                &mut ids,
                &mut all_reference_roots,
                protocols,
            )
            .await?,
        );
    }
    for step in &mut steps {
        if let Some(id) = &step.id
            && all_reference_roots.contains(id)
        {
            step.referenced_later = true;
        }
    }
    Ok(steps)
}

/// Validates one planned step: shape, address, protocol support and loaded
/// help, `id` uniqueness, the `for` spec, the `if` condition, and every
/// reference naming an earlier step or `for` variable. The step's `id` joins
/// `ids` only after its own references are checked, and every reference root
/// is recorded in `all_reference_roots` for the referenced-later pass above.
async fn validate_step(
    number: usize,
    raw_step: &Value,
    ids: &mut HashSet<String>,
    all_reference_roots: &mut Vec<String>,
    protocols: &ProtocolRegistry,
) -> Result<ValidatedStep> {
    check_step_shape(number, raw_step)?;
    let step: StepArgument = serde_json::from_value(raw_step.clone())
        .map_err(|error| anyhow!("invalid protocol arguments: step {number}: {error}"))?;
    let (operation, address, field) = match (&step.read, &step.exec) {
        (Some(_), Some(_)) => bail!(
            "invalid protocol arguments: step {number}: specify exactly one of `read` and \
                 `exec`; example: {STEP_SHAPE_EXAMPLE}"
        ),
        (None, None) => bail!(
            "invalid protocol arguments: step {number}: specify one of `read` or `exec`; \
                 example: {STEP_SHAPE_EXAMPLE}"
        ),
        (Some(address), None) => (ProtocolOperation::Read, address, "read"),
        (None, Some(address)) => (ProtocolOperation::Exec, address, "exec"),
    };
    if address.trim().is_empty() {
        return Err(step_field_error(
            number,
            field,
            "the address cannot be empty",
        ));
    }
    let (protocol_name, target) = split_address(address).map_err(|error| {
        step_field_error(
            number,
            field,
            format!("{error}; example: {STEP_SHAPE_EXAMPLE}"),
        )
    })?;
    if !protocol_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || protocol_name.is_empty()
    {
        return Err(step_field_error(
            number,
            field,
            format!(
                "the protocol name {protocol_name:?} is invalid; addresses are \
                     `<protocol>://<target>`"
            ),
        ));
    }
    let exec = operation == ProtocolOperation::Exec;
    protocols
        .validate_step_operation(protocol_name, target, exec)
        .await
        .map_err(|error| step_field_error_from(number, field, error))?;
    let literal_fields = protocols.literal_input_fields(protocol_name).await;

    let id = match &step.id {
        Some(id) => {
            if !valid_id(id) {
                return Err(step_field_error(
                    number,
                    "id",
                    format!(
                        "{id:?} must match `[a-z][a-z0-9_]*`, must not be one of not, true, \
                             false, or null, and must be unique within the call"
                    ),
                ));
            }
            // The id joins `ids` only after this step's own references
            // are checked, so a step cannot reference itself.
            if ids.contains(id) {
                return Err(step_field_error(
                    number,
                    "id",
                    format!("{id:?} is already used by an earlier step"),
                ));
            }
            Some(id.clone())
        }
        None => None,
    };

    let show = match step.show.as_deref() {
        None | Some("all") => Show::All,
        Some("errors") => Show::Errors,
        Some("none") => Show::None,
        Some(other) => {
            return Err(step_field_error(
                number,
                "show",
                format!("{other:?} is not one of \"all\", \"errors\", or \"none\""),
            ));
        }
    };

    let loop_spec = match (&step.r#for, step.max) {
        (Some(text), Some(max)) => {
            if !(1..=MAX_FOR_ELEMENTS as i64).contains(&max) {
                return Err(step_field_error(
                    number,
                    "max",
                    format!("{max} is outside 1..={MAX_FOR_ELEMENTS}"),
                ));
            }
            let (variable, source_text) = text.split_once(" in ").ok_or_else(|| {
                step_field_error(
                    number,
                    "for",
                    format!("{text:?} must be `<name> in <reference>`"),
                )
            })?;
            let variable = variable.trim();
            if !valid_id(variable) {
                return Err(step_field_error(
                    number,
                    "for",
                    format!(
                        "the variable name {variable:?} must match `[a-z][a-z0-9_]*` and must \
                             not be one of not, true, false, or null"
                    ),
                ));
            }
            if ids.contains(variable) || id.as_deref() == Some(variable) {
                return Err(step_field_error(
                    number,
                    "for",
                    format!(
                        "the variable name {variable:?} is already a step `id`; choose a \
                             different name"
                    ),
                ));
            }
            let source = parse_operand_str(source_text.trim())
                .map_err(|error| step_field_error(number, "for", format!("{error:#}")))?;
            let Operand::Reference { root, .. } = &source else {
                return Err(step_field_error(
                    number,
                    "for",
                    "the source must be a reference to an earlier step, for example \
                         `issue in issues.json.items`",
                ));
            };
            check_reference_root(root, number, "for", &ids, None)?;
            Some(LoopSpec {
                variable: variable.to_string(),
                source,
            })
        }
        (Some(_), None) => {
            return Err(step_field_error(
                number,
                "for",
                "`for` requires `max`, the maximum number of elements",
            ));
        }
        (None, Some(_)) => {
            return Err(step_field_error(number, "max", "`max` requires `for`"));
        }
        (None, None) => None,
    };
    let max = step.max.unwrap_or(0).max(0) as usize;

    let condition = match &step.r#if {
        Some(text) => {
            let expression = parse_expression(text)
                .map_err(|error| step_field_error(number, "if", format!("{error:#}")))?;
            for root in expression.roots() {
                let loop_variable = loop_spec.as_ref().map(|spec| spec.variable.as_str());
                check_reference_root(root, number, "if", &ids, loop_variable)?;
            }
            Some(expression)
        }
        None => None,
    };

    let loop_variable = loop_spec.as_ref().map(|spec| spec.variable.as_str());
    let mut data_refs = Vec::new();
    if let Some(input) = &step.input {
        for (key, value) in input {
            if !literal_fields.contains(key) {
                collect_input_roots(value, &mut data_refs);
            }
        }
        for root in &data_refs {
            check_reference_root(root, number, "input", &ids, loop_variable)?;
        }
    }
    for (_, _, inner) in address_placeholders(address)
        .map_err(|error| step_field_error(number, field, format!("{error:#}")))?
    {
        let operand = parse_operand_str(&inner)
            .map_err(|error| step_field_error(number, field, format!("{error:#}")))?;
        if let Some(root) = operand.root() {
            check_reference_root(root, number, field, &ids, loop_variable)?;
            data_refs.push(root.to_string());
        }
    }
    if let Some(spec) = &loop_spec
        && let Operand::Reference { root, .. } = &spec.source
    {
        data_refs.push(root.clone());
    }
    if let Some(condition) = &condition {
        for root in condition.roots() {
            all_reference_roots.push(root.to_string());
        }
    }
    all_reference_roots.extend(data_refs.iter().cloned());
    if let Some(id) = &id {
        ids.insert(id.clone());
    }

    Ok(ValidatedStep {
        operation,
        address: address.clone(),
        input: step.input,
        literal_fields,
        id,
        condition,
        loop_spec,
        max,
        show,
        data_refs,
        referenced_later: false,
    })
}
fn collect_input_roots(value: &Value, roots: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if let Some(operand) = whole_placeholder(text)
                && let Some(root) = operand.root()
            {
                roots.push(root.to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_input_roots(item, roots);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_input_roots(value, roots);
            }
        }
        _ => {}
    }
}

/// Per-step state used for skip propagation and `if` conditions.
#[derive(Clone, Debug, PartialEq)]
enum StepState {
    Ran { ok: bool },
    Failed,
    Skipped,
}

impl StepState {
    fn unusable(&self) -> bool {
        matches!(self, Self::Failed | Self::Skipped)
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Skipped => "was skipped",
            Self::Ran { .. } => "is usable",
        }
    }
}

fn skipped_value() -> Value {
    serde_json::json!({ "ok": false, "text": "", "json": null })
}

fn operation_value(ok: bool, text: String, json: Option<Value>) -> Value {
    serde_json::json!({
        "ok": ok,
        "text": text,
        "json": json.unwrap_or(Value::Null),
    })
}

async fn execute_steps(
    steps: &[ValidatedStep],
    protocols: &ProtocolRegistry,
) -> Result<ModelToolOutput> {
    let total = steps.len();
    let mut sections: Vec<String> = Vec::new();
    let mut images = Vec::new();
    let mut values: HashMap<String, Value> = HashMap::new();
    let mut states: HashMap<String, StepState> = HashMap::new();
    let mut operations_started = 0usize;

    for (index, step) in steps.iter().enumerate() {
        let number = index + 1;
        if let Some(root) = step
            .data_refs
            .iter()
            .find(|root| states.get(*root).is_some_and(|state| state.unusable()))
        {
            let reason = states
                .get(root)
                .map(|state| state.reason())
                .unwrap_or("failed");
            sections.push(format!(
                "*** Result {number} of {total}: skipped\nthis step references step `{root}`, \
                 which {reason}"
            ));
            if let Some(id) = &step.id {
                values.insert(id.clone(), skipped_value());
                states.insert(id.clone(), StepState::Skipped);
            }
            continue;
        }
        if let Some(spec) = &step.loop_spec {
            let source = match &spec.source {
                Operand::Reference { root, segments } => resolve_reference(root, segments, &values),
                Operand::Literal(_) => Value::Null,
            };
            let Value::Array(list) = source else {
                sections.push(format!(
                    "*** Result {number} of {total}: error\n`for` source did not resolve to a list"
                ));
                if let Some(id) = &step.id {
                    values.insert(id.clone(), skipped_value());
                    states.insert(id.clone(), StepState::Failed);
                }
                continue;
            };
            if list.len() > step.max {
                sections.push(format!(
                    "*** Result {number} of {total}: error\n`for` list has {} elements, more than `max` = {}",
                    list.len(),
                    step.max
                ));
                if let Some(id) = &step.id {
                    values.insert(id.clone(), skipped_value());
                    states.insert(id.clone(), StepState::Failed);
                }
                continue;
            }
            let mut element_values = Vec::new();
            let mut any_failed = false;
            let mut limited = false;
            if list.is_empty() {
                sections.push(format!(
                    "*** Result {number} of {total}: ok\n(the `for` list is empty)"
                ));
            }
            for (element_index, element) in list.iter().enumerate() {
                let label = format!("{number}.{}", element_index + 1);
                values.insert(spec.variable.clone(), element.clone());
                match run_element(
                    step,
                    &values,
                    protocols,
                    &mut operations_started,
                    total,
                    &label,
                    &mut sections,
                    &mut images,
                )
                .await
                {
                    ElementOutcome::Ran { ok, text, json } => {
                        any_failed |= !ok;
                        element_values.push(operation_value(ok, text, json));
                    }
                    ElementOutcome::Skipped => element_values.push(skipped_value()),
                    ElementOutcome::Limited => {
                        limited = true;
                        element_values.push(skipped_value());
                    }
                }
            }
            values.remove(&spec.variable);
            if let Some(id) = &step.id {
                // A list cut short by the operation limit is incomplete, so
                // later steps that use it as data are skipped.
                states.insert(
                    id.clone(),
                    if any_failed {
                        StepState::Failed
                    } else if limited {
                        StepState::Skipped
                    } else {
                        StepState::Ran { ok: true }
                    },
                );
                values.insert(id.clone(), Value::Array(element_values));
            }
        } else {
            match run_element(
                step,
                &values,
                protocols,
                &mut operations_started,
                total,
                &number.to_string(),
                &mut sections,
                &mut images,
            )
            .await
            {
                ElementOutcome::Ran { ok, text, json } => {
                    if let Some(id) = &step.id {
                        // A failed operation leaves `.ok` and `.text`
                        // readable for `if` conditions, but later steps that
                        // reference it as data are skipped.
                        states.insert(
                            id.clone(),
                            if ok {
                                StepState::Ran { ok: true }
                            } else {
                                StepState::Failed
                            },
                        );
                        values.insert(id.clone(), operation_value(ok, text, json));
                    }
                }
                ElementOutcome::Skipped | ElementOutcome::Limited => {
                    if let Some(id) = &step.id {
                        states.insert(id.clone(), StepState::Skipped);
                        values.insert(id.clone(), skipped_value());
                    }
                }
            }
        }
    }

    Ok(ModelToolOutput::new(sections.join("\n\n"), images))
}

enum ElementOutcome {
    Ran {
        ok: bool,
        text: String,
        json: Option<Value>,
    },
    /// Skipped by its `if` condition.
    Skipped,
    /// Skipped because the call reached `MAX_OPERATIONS`.
    Limited,
}

#[allow(clippy::too_many_arguments)]
async fn run_element(
    step: &ValidatedStep,
    values: &HashMap<String, Value>,
    protocols: &ProtocolRegistry,
    operations_started: &mut usize,
    total: usize,
    label: &str,
    sections: &mut Vec<String>,
    images: &mut Vec<crate::protocol::ProtocolImage>,
) -> ElementOutcome {
    if *operations_started >= MAX_OPERATIONS {
        sections.push(format!(
            "*** Result {label} of {total}: skipped\nthis call reached the limit of \
             {MAX_OPERATIONS} protocol operations"
        ));
        return ElementOutcome::Limited;
    }
    let failed = |sections: &mut Vec<String>, error: anyhow::Error| {
        let message = format!("{error:#}");
        if step.show.shows_output(true) {
            sections.push(format!("*** Result {label} of {total}: error\n{message}"));
        } else {
            sections.push(format!("*** Result {label} of {total}: error"));
        }
        ElementOutcome::Ran {
            ok: false,
            text: message,
            json: None,
        }
    };
    if let Some(condition) = &step.condition {
        let decision = match eval_expression(condition, values) {
            Ok(decision) => decision,
            Err(error) => return failed(sections, error),
        };
        if !decision {
            sections.push(format!(
                "*** Result {label} of {total}: skipped\nthe `if` condition is false"
            ));
            return ElementOutcome::Skipped;
        }
    }
    let address = match substitute_address(&step.address, values) {
        Ok(address) => address,
        Err(error) => return failed(sections, error),
    };
    let input = match &step.input {
        Some(input) => match substitute_input(input, values, &step.literal_fields) {
            Ok(input) => input,
            Err(error) => return failed(sections, error),
        },
        None => Map::new(),
    };
    *operations_started += 1;
    let result = match step.operation {
        ProtocolOperation::Read => {
            protocols
                .read_for_model(&address, &input, step.referenced_later)
                .await
        }
        ProtocolOperation::Exec => {
            protocols
                .exec_for_model(&address, &input, step.referenced_later)
                .await
        }
    };
    match result {
        Ok(output) => {
            if step.show.shows_output(false) && !output.images.is_empty() {
                images.extend(output.images);
            }
            if step.show.shows_output(false) {
                sections.push(format!(
                    "*** Result {label} of {total}: ok\n{}",
                    output.output
                ));
            } else {
                sections.push(format!("*** Result {label} of {total}: ok"));
            }
            ElementOutcome::Ran {
                ok: true,
                text: output.text,
                json: output.json,
            }
        }
        Err(error) => failed(sections, error),
    }
}

#[async_trait]
impl ModelTool for HelpTool {
    fn descriptor(&self) -> ModelToolDescriptor {
        ModelToolDescriptor {
            name: "help".to_string(),
            description: prompts::HELP_TOOL_DESCRIPTION.to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "protocols": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Names of protocols to load from the Available protocols list, for example [\"file\", \"search\"]. Shared prerequisites such as the MCP routing page are included automatically."
                    }
                },
                "required": ["protocols"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        arguments: &Value,
        protocols: &ProtocolRegistry,
    ) -> Result<ModelToolOutput> {
        let arguments: HelpArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| anyhow!("invalid help arguments: {error}"))?;
        Ok(protocols.load_help(&arguments.protocols).await?.into())
    }
}

pub(super) struct ProtocolToolsPlugin;

pub(crate) fn register_protocol_tools(
    registry: &mut crate::plugin::ModelToolRegistry,
) -> Result<()> {
    registry.register(ProtocolTool)?;
    registry.register(HelpTool)
}

impl Plugin for ProtocolToolsPlugin {
    fn model_tool_descriptors(&self) -> Vec<ModelToolDescriptor> {
        [ProtocolTool.descriptor(), HelpTool.descriptor()].to_vec()
    }

    fn register(&self, host: &mut PluginHost<'_>) -> Result<()> {
        register_protocol_tools(host.model_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentHandle, AgentHost, AgentSpec};
    use crate::catalog::ModelCatalog;
    use crate::config::{AgentEnvironment, ConfigManager};
    use crate::output::OutputStore;
    use crate::protocol::{
        Protocol, ProtocolContext, ProtocolDescriptor, ProtocolOutput, ProtocolRequest,
    };
    use crate::task::{AutoTask, TaskManager};
    use anyhow::anyhow;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Recorded protocol calls: `(target, input)` pairs.
    type RecordedInputs = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;

    const FAKE_HELP: &str = r#"# fake

A recording protocol for step tests.

```json
{"read": "fake://data"}
```
"#;

    /// Records every operation and answers by target: `data` returns a fixed
    /// JSON document, `echo` returns its input as JSON, `fail` fails, and
    /// anything else records the call and echoes the input.
    struct FakeProtocol {
        calls: RecordedInputs,
    }

    #[async_trait]
    impl Protocol for FakeProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "fake".to_string(),
                description: "recording protocol for tests".to_string(),
                can_read: true,
                can_exec: true,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            self.answer(request)
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            self.answer(request)
        }
    }

    impl FakeProtocol {
        fn answer(&self, request: ProtocolRequest<'_>) -> Result<ProtocolOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((request.target.to_string(), request.input.clone()));
            match request.target {
                "help" => Ok(ProtocolOutput::text(FAKE_HELP.as_bytes().to_vec())),
                "data" => Ok(ProtocolOutput::new(
                    b"DATA".to_vec(),
                    Some(json!({"items": ["a", "b", "c"], "count": 3, "path": "src/a.rs"})),
                    Vec::new(),
                )),
                "fail" => Err(anyhow!("boom")),
                _ => {
                    let text = serde_json::to_string(&request.input).unwrap();
                    Ok(ProtocolOutput::new(
                        text.as_bytes().to_vec(),
                        Some(Value::Object(request.input.clone())),
                        Vec::new(),
                    ))
                }
            }
        }
    }

    /// Auto-backgrounds under any nonzero grace like the shell: unreferenced
    /// operations promote to a background task, referenced ones stay in the
    /// foreground.
    struct SlowpokeProtocol {
        pinned: Arc<Mutex<Vec<bool>>>,
    }

    #[async_trait]
    impl Protocol for SlowpokeProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "slowpoke".to_string(),
                description: "auto-backgrounding protocol for tests".to_string(),
                can_read: true,
                can_exec: true,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            if request.target == "help" {
                request.reject_input()?;
                return Ok(ProtocolOutput::text("slowpoke help".as_bytes().to_vec()));
            }
            bail!("slowpoke supports only exec")
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            if request.target == "help" {
                request.reject_input()?;
                return Ok(ProtocolOutput::text("slowpoke help".as_bytes().to_vec()));
            }
            self.pinned.lock().unwrap().push(context.pinned_foreground);
            let record = context
                .tasks
                .allocate("slowpoke", "slowpoke job".to_string())
                .await;
            match context
                .tasks
                .run_with_auto_background(
                    record,
                    context.foreground_grace(Duration::ZERO),
                    |_| async { Ok(b"done".to_vec()) },
                )
                .await?
            {
                AutoTask::Background(_) => {
                    Ok(ProtocolOutput::text(b"background-promoted".to_vec()))
                }
                AutoTask::Terminal(record) => Ok(ProtocolOutput::text(
                    format!("foreground-{}", String::from_utf8_lossy(&record.content)).into_bytes(),
                )),
            }
        }
    }

    /// A protocol whose `script` field is executed text and must never be
    /// substituted.
    struct ScriptProtocol {
        calls: Arc<Mutex<Vec<Map<String, Value>>>>,
    }

    #[async_trait]
    impl Protocol for ScriptProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: "scripted".to_string(),
                description: "script protocol for tests".to_string(),
                can_read: true,
                can_exec: true,
            }
        }

        fn literal_input_fields(&self) -> &[&str] {
            &["script"]
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            if request.target == "help" {
                request.reject_input()?;
                return Ok(ProtocolOutput::text("scripted help".as_bytes().to_vec()));
            }
            bail!("scripted supports only exec")
        }

        async fn exec(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            if request.target == "help" {
                bail!("scripted help is read-only");
            }
            self.calls.lock().unwrap().push(request.input.clone());
            Ok(ProtocolOutput::text("script-ok".as_bytes().to_vec()))
        }
    }

    struct Harness {
        registry: ProtocolRegistry,
        calls: RecordedInputs,
        script_calls: Arc<Mutex<Vec<Map<String, Value>>>>,
        pinned: Arc<Mutex<Vec<bool>>>,
        /// Non-help reads of the `readonly` and `unloaded` protocols.
        limited_calls: Arc<Mutex<usize>>,
        output_directory: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.output_directory);
        }
    }

    async fn new_harness() -> Harness {
        let session_id = format!("steps-test{}", uuid::Uuid::now_v7().simple());
        let output = Arc::new(OutputStore::new(&session_id, 1024).await.unwrap());
        let output_directory = output.directory().to_path_buf();
        let mut registry = ProtocolRegistry::new(output, TaskManager::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let script_calls = Arc::new(Mutex::new(Vec::new()));
        let pinned = Arc::new(Mutex::new(Vec::new()));
        registry
            .register(FakeProtocol {
                calls: calls.clone(),
            })
            .unwrap();
        registry
            .register(SlowpokeProtocol {
                pinned: pinned.clone(),
            })
            .unwrap();
        registry
            .register(ScriptProtocol {
                calls: script_calls.clone(),
            })
            .unwrap();
        let limited_calls = Arc::new(Mutex::new(0));
        for (name, can_exec) in [("readonly", false), ("unloaded", true)] {
            registry
                .register(LimitedProtocol {
                    name,
                    can_exec,
                    calls: limited_calls.clone(),
                })
                .unwrap();
        }
        // `unloaded` stays without loaded help for the help-gate tests.
        registry
            .load_help(&[
                "fake".to_string(),
                "slowpoke".to_string(),
                "scripted".to_string(),
                "readonly".to_string(),
            ])
            .await
            .unwrap();
        calls.lock().unwrap().clear();
        Harness {
            registry,
            calls,
            script_calls,
            pinned,
            limited_calls,
            output_directory,
        }
    }

    async fn run(harness: &Harness, steps: Value) -> Result<ModelToolOutput> {
        ProtocolTool
            .execute(&json!({ "steps": steps }), &harness.registry)
            .await
    }

    fn recorded(harness: &Harness) -> Vec<(String, Map<String, Value>)> {
        harness.calls.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn single_step_reports_result_one_of_one() {
        let harness = new_harness().await;
        let output = run(&harness, json!([{"read": "fake://data"}]))
            .await
            .unwrap();
        assert_eq!(output.output(), "*** Result 1 of 1: ok\nDATA");
        assert_eq!(recorded(&harness).len(), 1);
    }

    #[tokio::test]
    async fn validation_rejects_the_whole_call_before_running_anything() {
        let harness = new_harness().await;
        let error = run(
            &harness,
            json!([
                {"read": "fake://data"},
                {"read": "fake://data", "unknown": true}
            ]),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("step 2"));
        assert!(
            recorded(&harness).is_empty(),
            "a rejected call must run nothing"
        );

        for (steps, fragment) in [
            (json!([]), "at least one step is required"),
            (
                Value::Array(vec![json!({"read": "fake://data"}); 9]),
                "at most 8 steps per call",
            ),
            (json!([{"read": "   "}]), "the address cannot be empty"),
            (
                json!([{"read": "fake://data", "id": "Bad-ID"}]),
                "must match `[a-z][a-z0-9_]*`",
            ),
            (
                json!([
                    {"id": "a", "read": "fake://data"},
                    {"id": "a", "read": "fake://data"}
                ]),
                "already used by an earlier step",
            ),
            (
                json!([{"read": "fake://data", "show": "sometimes"}]),
                "is not one of",
            ),
            (
                json!([{"read": "fake://data", "if": "later.ok"}]),
                "does not name an earlier step",
            ),
            (
                json!([{"read": "fake://data", "if": "(("}]),
                "invalid operand",
            ),
            (
                json!([{"for": "x in a.items", "read": "fake://data"}]),
                "`for` requires `max`",
            ),
            (
                json!([{"read": "fake://data", "max": 3}]),
                "`max` requires `for`",
            ),
            (
                json!([{"for": "x in 3", "max": 3, "read": "fake://data"}]),
                "the source must be a reference",
            ),
        ] {
            let error = format!("{:#}", run(&harness, steps).await.unwrap_err());
            assert!(error.contains(fragment), "expected {fragment:?} in {error}");
            assert!(recorded(&harness).is_empty());
        }
    }

    #[tokio::test]
    async fn for_runs_once_per_element_in_order_and_exposes_the_element_list() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {
                    "id": "letters",
                    "for": "item in d.json.items",
                    "max": 8,
                    "read": "fake://{{ item }}",
                    "input": {"element": "{{ item }}"}
                },
                {"read": "fake://echo", "input": {"list": "{{ letters }}"}}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert!(text.contains("*** Result 2.1 of 3: ok"));
        assert!(text.contains("*** Result 2.3 of 3: ok"));
        let calls = recorded(&harness);
        let targets: Vec<&str> = calls.iter().map(|(target, _)| target.as_str()).collect();
        assert_eq!(
            targets,
            ["data", "a", "b", "c", "echo"],
            "elements run sequentially in order"
        );
        // The last call receives the list of per-element values.
        let last_input = calls.last().unwrap().1.clone();
        let list = last_input.get("list").unwrap();
        let texts: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.get("text").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "{\"element\":\"a\"}",
                "{\"element\":\"b\"}",
                "{\"element\":\"c\"}"
            ]
        );
    }

    #[tokio::test]
    async fn for_sources_must_be_lists_within_max_before_any_element_runs() {
        let harness = new_harness().await;
        let not_a_list = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"for": "x in d.json.count", "max": 4, "read": "fake://echo"}
            ]),
        )
        .await
        .unwrap();
        assert!(
            not_a_list
                .output()
                .contains("`for` source did not resolve to a list")
        );

        let harness = new_harness().await;
        let oversized = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"for": "x in d.json.items", "max": 2, "read": "fake://{{ x }}"}
            ]),
        )
        .await
        .unwrap();
        assert!(
            oversized
                .output()
                .contains("`for` list has 3 elements, more than `max` = 2"),
            "{}",
            oversized.output()
        );
        assert_eq!(
            recorded(&harness).len(),
            1,
            "an oversized list fails before any element runs"
        );

        let harness = new_harness().await;
        let empty = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://echo", "show": "none", "input": {"items": []}},
                {"for": "x in d.json.items", "max": 4, "read": "fake://echo", "input": {"list": "{{ x }}"}},
                {"read": "fake://echo", "input": {"after": "{{ d.json.items }}"}}
            ]),
        )
        .await
        .unwrap();
        assert!(
            empty
                .output()
                .contains("*** Result 2 of 3: ok\n(the `for` list is empty)")
        );
        let calls = recorded(&harness);
        assert_eq!(calls.len(), 2, "the loop body never runs for an empty list");
        let last = calls.last().unwrap().1.clone();
        assert_eq!(
            last.get("after").unwrap(),
            &json!([]),
            "the loop step's own value is the empty element list"
        );
    }

    #[tokio::test]
    async fn loop_variables_are_invisible_outside_their_step() {
        let harness = new_harness().await;
        let error = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"for": "item in d.json.items", "max": 4, "read": "fake://data"},
                {"read": "fake://echo", "input": {"late": "{{ item }}"}}
            ]),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("does not name an earlier step"),
            "the loop variable must not leak: {error}"
        );
    }

    #[tokio::test]
    async fn failed_and_skipped_steps_are_reported_without_stopping_the_call() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"read": "fake://fail"},
                {"read": "fake://data"},
                {"id": "unused", "if": "false", "read": "fake://data"},
                {"if": "true", "read": "fake://data"}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert!(text.contains("*** Result 1 of 4: error\nboom"));
        assert!(text.contains("*** Result 2 of 4: ok\nDATA"));
        assert!(text.contains("*** Result 3 of 4: skipped\nthe `if` condition is false"));
        assert!(text.contains("*** Result 4 of 4: ok\nDATA"));
    }

    #[tokio::test]
    async fn referencing_a_failed_or_skipped_step_skips_with_that_reason() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "broken", "read": "fake://fail", "show": "none"},
                {"read": "fake://echo", "input": {"value": "{{ broken.text }}"}},
                {"id": "off", "if": "false", "read": "fake://data"},
                {"read": "fake://echo", "input": {"value": "{{ off.json }}"}},
                {"if": "broken.ok == false", "read": "fake://data"}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert!(
            text.contains(
                "*** Result 2 of 5: skipped\nthis step references step `broken`, which failed"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "*** Result 4 of 5: skipped\nthis step references step `off`, which was skipped"
            ),
            "{text}"
        );
        assert!(
            text.contains("*** Result 5 of 5: ok\nDATA"),
            "an `if` may read `.ok` of a failed step: {text}"
        );
        assert_eq!(recorded(&harness).len(), 2, "only steps 1 and 5 run");
    }

    #[tokio::test]
    async fn whole_value_placeholders_keep_the_json_type() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"read": "fake://echo", "input": {
                    "count": "{{ d.json.count }}",
                    "path": "{{ d.json.path }}",
                    "items": "{{ d.json.items }}"
                }}
            ]),
        )
        .await
        .unwrap();
        let calls = recorded(&harness);
        let input = calls.last().unwrap().1.clone();
        assert_eq!(input.get("count"), Some(&json!(3)), "numbers stay numbers");
        assert_eq!(input.get("path"), Some(&json!("src/a.rs")));
        assert_eq!(input.get("items"), Some(&json!(["a", "b", "c"])));
        let text = output.output();
        assert!(text.contains("*** Result 2 of 2: ok"));
    }

    #[tokio::test]
    async fn partial_placeholder_strings_stay_literal() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"read": "fake://echo", "input": {
                    "secret": "${{ d.json.path }}",
                    "embedded": "value {{ d.json.count }} here",
                    "escaped": "{{ \"{{x}}\" }}"
                }}
            ]),
        )
        .await
        .unwrap();
        let calls = recorded(&harness);
        let input = calls.last().unwrap().1.clone();
        assert_eq!(input.get("secret"), Some(&json!("${{ d.json.path }}")));
        assert_eq!(
            input.get("embedded"),
            Some(&json!("value {{ d.json.count }} here"))
        );
        assert_eq!(input.get("escaped"), Some(&json!("{{x}}")));
        assert!(output.output().contains("*** Result 2 of 2: ok"));
    }

    #[tokio::test]
    async fn address_placeholders_accept_scalars_and_reject_containers() {
        let harness = new_harness().await;
        run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"read": "fake://{{ d.json.path }}"}
            ]),
        )
        .await
        .unwrap();
        assert_eq!(recorded(&harness).last().unwrap().0, "src/a.rs");

        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"read": "fake://run-{{ d.json.items }}"}
            ]),
        )
        .await
        .unwrap();
        assert!(
            output
                .output()
                .contains("address placeholders must reference a string, number, or boolean"),
            "{}",
            output.output()
        );
    }

    #[tokio::test]
    async fn literal_input_fields_are_never_substituted() {
        let harness = new_harness().await;
        run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {
                    "exec": "scripted://run",
                    "input": {
                        "script": "{{ d.json.path }}",
                        "env": {"TARGET": "{{ d.json.path }}", "COUNT": "{{ d.json.count }}"}
                    }
                }
            ]),
        )
        .await
        .unwrap();
        let calls = harness.script_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        let input = &calls[0];
        assert_eq!(
            input.get("script"),
            Some(&json!("{{ d.json.path }}")),
            "executed text keeps its placeholders"
        );
        assert_eq!(
            input.get("env").unwrap().get("TARGET"),
            Some(&json!("src/a.rs"))
        );
        assert_eq!(input.get("env").unwrap().get("COUNT"), Some(&json!(3)));
    }

    #[tokio::test]
    async fn operations_stop_at_sixty_four_and_report_skipped_elements() {
        let harness = new_harness().await;
        let items = Value::Array(vec![json!("x"); 32]);
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://echo", "show": "none", "input": {"items": items}},
                {"for": "item in d.json.items", "max": 32, "read": "fake://echo", "input": {"item": "{{ item }}"}},
                {"for": "item in d.json.items", "max": 32, "read": "fake://echo", "input": {"item": "{{ item }}"}}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert!(
            text.contains("*** Result 3.31 of 3: ok"),
            "the sixty-fourth operation still runs: {text}"
        );
        assert!(
            text.contains(
                "*** Result 3.32 of 3: skipped\nthis call reached the limit of 64 protocol operations"
            ),
            "{text}"
        );
        assert_eq!(recorded(&harness).len(), 64);
    }

    #[tokio::test]
    async fn operations_referenced_later_stay_in_the_foreground() {
        let harness = new_harness().await;
        let referenced = run(
            &harness,
            json!([
                {"id": "slow", "exec": "slowpoke://job"},
                {"read": "fake://echo", "input": {"ok": "{{ slow.ok }}"}}
            ]),
        )
        .await
        .unwrap();
        assert!(
            referenced.output().contains("foreground-done"),
            "{}",
            referenced.output()
        );
        assert_eq!(*harness.pinned.lock().unwrap(), [true]);

        let harness = new_harness().await;
        let unreferenced = run(
            &harness,
            json!([{"exec": "slowpoke://job"}, {"read": "fake://data"}]),
        )
        .await
        .unwrap();
        assert!(
            unreferenced.output().contains("background-promoted"),
            "{}",
            unreferenced.output()
        );
        assert_eq!(*harness.pinned.lock().unwrap(), [false]);
    }

    #[tokio::test]
    async fn show_controls_what_enters_the_result() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"read": "fake://data", "show": "none"},
                {"read": "fake://fail", "show": "none"},
                {"read": "fake://fail", "show": "errors"},
                {"read": "fake://data", "show": "errors"}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert_eq!(
            text,
            "*** Result 1 of 4: ok\n\n\
             *** Result 2 of 4: error\n\n\
             *** Result 3 of 4: error\nboom\n\n\
             *** Result 4 of 4: ok"
        );
    }

    /// The real root-session registry, so help-example validation runs
    /// against the built-in protocols this environment actually registers.
    /// The handle must stay alive: the collaboration protocol holds only a
    /// weak reference to the runtime it renders its help from.
    async fn builtin_registry() -> (tempfile::TempDir, AgentHandle, Arc<ProtocolRegistry>) {
        let workspace = tempfile::tempdir().unwrap();
        let config_directory = workspace.path().join("config");
        tokio::fs::create_dir_all(&config_directory).await.unwrap();
        tokio::fs::write(
            config_directory.join("models.json"),
            br#"{"providers":{"test-provider":{"baseUrl":"https://test.invalid/v1","api":"openai-responses","models":[{"id":"test-model","name":"Test","maxTokens":9000}]}}}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(config_directory.join("settings.json"), b"{}")
            .await
            .unwrap();
        let manager = ConfigManager::load_for_test(&config_directory, workspace.path())
            .await
            .unwrap();
        let environment = Arc::new(AgentEnvironment::load(&config_directory).await.unwrap());
        let catalog = Arc::new(ModelCatalog::load(&config_directory, true).await.unwrap());
        let host = AgentHost::new(
            manager.clone(),
            environment,
            catalog,
            workspace.path().to_path_buf(),
        )
        .await
        .unwrap();
        let initial = manager.current().await;
        let spec = AgentSpec::root(
            &initial.provider,
            &initial.model,
            initial.thinking,
            workspace.path(),
        );
        let handle = host.open_root(None, spec).await.unwrap();
        handle.services().runtime.prepare_context().await.unwrap();
        let protocols = handle.services().protocols.clone();
        (workspace, handle, protocols)
    }

    /// Extract every ```json fenced block, parsed.
    fn json_fences(text: &str) -> Vec<Value> {
        let mut examples = Vec::new();
        let mut rest = text;
        while let Some(position) = rest.find("```json") {
            let after = &rest[position + "```json".len()..];
            let end = after
                .find("```")
                .expect("help text has an unterminated json fence");
            let block = after[..end].trim();
            let value = serde_json::from_str(block)
                .unwrap_or_else(|error| panic!("invalid json fence {block:?}: {error}"));
            examples.push(value);
            rest = &after[end + "```".len()..];
        }
        examples
    }

    /// Extract every single-line step example outside ```json fences: a
    /// whole line or a backtick code span that parses as a JSON object starting with a
    /// step or `steps` field.
    fn inline_steps(text: &str) -> Vec<Value> {
        const STARTS: [&str; 4] = ["{\"read\"", "{\"exec\"", "{\"id\"", "{\"steps\""];
        let mut candidates = Vec::new();
        let mut in_json_fence = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_json_fence = !in_json_fence && trimmed == "```json";
                continue;
            }
            // `json_fences` covers fenced JSON, including multi-line calls.
            if in_json_fence {
                continue;
            }
            candidates.push(trimmed);
            candidates.extend(trimmed.split('`').skip(1).step_by(2));
        }
        candidates
            .into_iter()
            .filter(|candidate| STARTS.iter().any(|start| candidate.starts_with(start)))
            .map(|candidate| {
                serde_json::from_str(candidate)
                    .unwrap_or_else(|error| panic!("invalid inline step {candidate:?}: {error}"))
            })
            .collect()
    }

    /// Normalize one extracted example to its step array: help pages show a
    /// single step object, `docs/protocols.md` shows complete calls.
    fn example_steps(example: &Value) -> Vec<Value> {
        match example {
            Value::Object(object) if object.contains_key("steps") => object["steps"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| panic!("`steps` must be an array: {example}")),
            _ => vec![example.clone()],
        }
    }

    fn step_protocol_names(steps: &[Value]) -> Vec<String> {
        steps
            .iter()
            .map(|step| {
                let address = step
                    .as_object()
                    .and_then(|object| object.get("read").or_else(|| object.get("exec")))
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("json fence is not a step example: {step}"));
                address.split("://").next().unwrap().to_string()
            })
            .collect()
    }

    /// Every ```json step example on every registered help page and in
    /// `docs/protocols.md` must pass whole-call validation against the real
    /// registry. Examples naming a protocol this environment does not
    /// register, such as a configured MCP server, are skipped instead of
    /// failing.
    #[tokio::test]
    async fn help_page_and_docs_examples_validate_as_steps() {
        async fn check(
            steps: &[Value],
            registered: &HashSet<&str>,
            protocols: &ProtocolRegistry,
        ) -> bool {
            let targets = step_protocol_names(steps);
            if targets
                .iter()
                .any(|name| !registered.contains(name.as_str()))
            {
                return false;
            }
            validate_steps(steps, protocols)
                .await
                .unwrap_or_else(|error| panic!("example {steps:?} failed validation: {error:#}"));
            true
        }

        let (_workspace, _handle, protocols) = builtin_registry().await;
        // Skill protocols come from the developer's home directory, not this
        // repository, so their pages are not built-in examples.
        let names: Vec<String> = protocols
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .filter(|name| !name.ends_with("-skill"))
            .collect();
        assert!(
            names.iter().any(|name| name == "file"),
            "the built-in registry is missing core protocols: {names:?}"
        );

        // Load every contract first: validation requires the target
        // protocol's help to be loaded, and one page's examples may target
        // other protocols than the page they appear on. `load_help` takes at
        // most eight names per call.
        let mut pages = Vec::new();
        for chunk in names.chunks(8) {
            pages.push(protocols.load_help(chunk).await.unwrap());
        }

        let registered: HashSet<&str> = names.iter().map(String::as_str).collect();
        let mut validated = 0;
        for page in &pages {
            for example in json_fences(page).into_iter().chain(inline_steps(page)) {
                let steps = example_steps(&example);
                if check(&steps, &registered, &protocols).await {
                    validated += 1;
                }
            }
        }
        assert!(validated > 0, "no help-page examples were validated");

        let docs = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/protocols.md"),
        )
        .unwrap();
        let mut docs_validated = 0;
        for example in json_fences(&docs).into_iter().chain(inline_steps(&docs)) {
            let steps = example_steps(&example);
            if check(&steps, &registered, &protocols).await {
                docs_validated += 1;
            }
        }
        assert!(
            docs_validated > 0,
            "no docs/protocols.md examples were validated"
        );
    }

    /// The `protocol` tool schema is pinned to the step shape providers see:
    /// one required `steps` array of 1..=8 objects, fixed step field types,
    /// unknown step properties rejected, and no union constructs anywhere.
    #[test]
    fn protocol_tool_schema_pins_the_step_shape() {
        let schema = ProtocolTool.descriptor().parameters;
        let root = schema.as_object().expect("tool schema is an object");
        assert_eq!(root.get("type").and_then(Value::as_str), Some("object"));
        assert_eq!(root.get("required"), Some(&json!(["steps"])));
        assert_eq!(root.get("additionalProperties"), Some(&json!(false)));
        let steps = &root["properties"]["steps"];
        assert_eq!(steps["type"], json!("array"));
        assert_eq!(steps["minItems"], json!(1));
        assert_eq!(steps["maxItems"], json!(MAX_STEPS));
        assert_eq!(steps["items"], json!({"$ref": "#/$defs/step"}));

        let step = &root["$defs"]["step"];
        assert_eq!(step["type"], json!("object"));
        assert_eq!(step["additionalProperties"], json!(false));
        let properties = step["properties"].as_object().expect("step properties");
        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["exec", "for", "id", "if", "input", "max", "read", "show"]
        );
        for (field, kind) in [
            ("read", "string"),
            ("exec", "string"),
            ("input", "object"),
            ("id", "string"),
            ("if", "string"),
            ("for", "string"),
            ("max", "integer"),
            ("show", "string"),
        ] {
            assert_eq!(properties[field]["type"], json!(kind), "step field {field}");
        }
        assert_eq!(properties["show"]["enum"], json!(["all", "errors", "none"]));

        fn assert_fixed(value: &Value, path: &str) {
            let Some(object) = value.as_object() else {
                return;
            };
            if let Some(kind) = object.get("type") {
                assert!(
                    kind.is_string(),
                    "schema {path}: `type` must be one fixed type"
                );
            }
            assert!(!object.contains_key("anyOf"), "schema {path}: no anyOf");
            assert!(!object.contains_key("oneOf"), "schema {path}: no oneOf");
            for (key, child) in object {
                assert_fixed(child, &format!("{path}.{key}"));
            }
        }
        assert_fixed(&schema, "schema");
    }

    /// A protocol with configurable read/exec support, used for operation
    /// and help-gate validation tests.
    struct LimitedProtocol {
        name: &'static str,
        can_exec: bool,
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Protocol for LimitedProtocol {
        fn descriptor(&self) -> ProtocolDescriptor {
            ProtocolDescriptor {
                name: self.name.to_string(),
                description: "limited protocol for tests".to_string(),
                can_read: true,
                can_exec: self.can_exec,
            }
        }

        async fn read(
            &self,
            request: ProtocolRequest<'_>,
            _context: ProtocolContext,
        ) -> Result<ProtocolOutput> {
            if request.target != "help" {
                *self.calls.lock().unwrap() += 1;
            }
            Ok(ProtocolOutput::text(b"limited".to_vec()))
        }
    }

    #[tokio::test]
    async fn every_rejected_shape_names_the_step_and_field_and_runs_nothing() {
        let harness = new_harness().await;
        let limited_calls = harness.limited_calls.clone();

        for (steps, fragments) in [
            (
                json!([{"read": "fake://data"}, {"read": "fake://data", "input": "query"}]),
                vec!["step 2 field `input`", "must be an object, got a string"],
            ),
            (
                json!([{"read": "fake://data", "max": "3"}]),
                vec!["step 1 field `max`", "must be an integer"],
            ),
            (
                json!([{"read": "fake://data", "exec": "fake://echo"}]),
                vec!["step 1", "exactly one of `read` and `exec`"],
            ),
            (
                json!([{"input": {}}]),
                vec!["step 1", "one of `read` or `exec`"],
            ),
            (
                json!([{"read": "fake://data", "unknown": true}]),
                vec!["step 1 field `unknown`", "unknown step field"],
            ),
            (json!(["fake://data"]), vec!["step 1 must be an object"]),
            (
                json!([
                    {"read": "fake://echo", "input": {"v": "{{ later.text }}"}},
                    {"id": "later", "read": "fake://data"}
                ]),
                vec!["step 1 field `input`", "does not name an earlier step"],
            ),
            (
                json!([{"id": "me", "if": "me.ok", "read": "fake://data"}]),
                vec!["step 1 field `if`", "does not name an earlier step"],
            ),
            (
                json!([{"id": "me", "read": "fake://echo", "input": {"v": "{{ me.text }}"}}]),
                vec!["step 1 field `input`", "does not name an earlier step"],
            ),
            (
                json!([
                    {"id": "x", "read": "fake://data"},
                    {"for": "x in x.json.items", "max": 4, "read": "fake://echo"}
                ]),
                vec!["step 2 field `for`", "already a step `id`"],
            ),
            (
                json!([{"id": "null", "read": "fake://data"}]),
                vec![
                    "step 1 field `id`",
                    "must not be one of not, true, false, or null",
                ],
            ),
            (
                json!([{"read": "fake://data", "if": "d.ok =! true"}]),
                vec!["step 1 field `if`"],
            ),
            (
                json!([
                    {"id": "d", "read": "fake://data"},
                    {"for": "x in d.json.items", "max": 0, "read": "fake://echo"}
                ]),
                vec!["step 2 field `max`", "outside 1..=32"],
            ),
            (
                json!([
                    {"id": "d", "read": "fake://data"},
                    {"for": "x in d.json.items", "max": 33, "read": "fake://echo"}
                ]),
                vec!["step 2 field `max`", "outside 1..=32"],
            ),
            (
                json!([{"read": "missing://data"}]),
                vec!["step 1 field `read`", "unknown protocol"],
            ),
            (
                json!([{"exec": "readonly://data"}]),
                vec!["step 1 field `exec`", "does not support exec"],
            ),
            (
                json!([{"read": "unloaded://data"}]),
                vec!["step 1 field `read`", "help"],
            ),
            (
                json!([{"read": "fake://data"}, {"read": "fake://help"}]),
                vec![
                    "step 2 field `read`",
                    "help pages are loaded with the help tool",
                ],
            ),
        ] {
            let error = format!("{:#}", run(&harness, steps.clone()).await.unwrap_err());
            for fragment in fragments {
                assert!(
                    error.contains(fragment),
                    "expected {fragment:?} for {steps} in {error}"
                );
            }
            assert!(recorded(&harness).is_empty(), "{steps} must run nothing");
            assert_eq!(*limited_calls.lock().unwrap(), 0);
        }
    }

    #[test]
    fn expressions_parse_without_spaces_escapes_and_keyword_prefixes() {
        let values = HashMap::from([
            (
                "d".to_string(),
                json!({"ok": false, "json": {"count": 3, "name": "a\"b"}}),
            ),
            ("nullable".to_string(), json!({"ok": true})),
        ]);
        for (text, expected) in [
            ("d.json.count>2", true),
            ("d.json.count>=4", false),
            ("d.ok==false", true),
            ("not d.ok", true),
            ("nullable.ok", true),
            ("true_value_missing.ok == null", true),
            (r#"d.json.name == "a\"b""#, true),
            (r#""a\nb" == "a\nb""#, true),
            (r#""a\\b" == "a\\b""#, true),
        ] {
            let expression =
                parse_expression(text).unwrap_or_else(|error| panic!("{text}: {error:#}"));
            assert_eq!(
                eval_expression(&expression, &values).unwrap(),
                expected,
                "{text}"
            );
        }
        let Expr {
            left: Operand::Literal(literal),
            ..
        } = parse_expression(r#""a\nb\\c\"d""#).unwrap()
        else {
            panic!("expected a string literal");
        };
        assert_eq!(literal, json!("a\nb\\c\"d"));
        assert!(parse_expression(r#""a\qb""#).is_err());
    }

    #[tokio::test]
    async fn conditions_read_references_and_show_hides_condition_errors() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"if": "d.json.count>2", "read": "fake://echo", "input": {"n": 1}},
                {"if": "d.json.count < 2", "read": "fake://echo", "input": {"n": 2}},
                {"if": "d.json.items", "read": "fake://echo", "show": "none"},
                {"if": "d.json.items", "read": "fake://echo"}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert!(text.contains("*** Result 2 of 5: ok"), "{text}");
        assert!(
            text.contains("*** Result 3 of 5: skipped\nthe `if` condition is false"),
            "{text}"
        );
        assert!(text.contains("*** Result 4 of 5: error\n\n"), "{text}");
        assert!(
            text.contains("*** Result 5 of 5: error\na condition without a comparator"),
            "{text}"
        );
        assert_eq!(recorded(&harness).len(), 2);
    }

    #[tokio::test]
    async fn template_like_strings_and_literal_fields_stay_literal() {
        let harness = new_harness().await;
        run(
            &harness,
            json!([
                {"id": "d", "read": "fake://data", "show": "none"},
                {"read": "fake://echo", "input": {
                    "pair": "{{a}} and {{b}}",
                    "handlebars": "{{#each items}}{{this}}{{/each}}",
                    "go": "{{ define \"t\" }}x{{ end }}"
                }},
                {"exec": "scripted://run", "input": {"script": "{{ missing.text }}"}}
            ]),
        )
        .await
        .unwrap();
        let input = recorded(&harness).last().unwrap().1.clone();
        assert_eq!(input["pair"], json!("{{a}} and {{b}}"));
        assert_eq!(
            input["handlebars"],
            json!("{{#each items}}{{this}}{{/each}}")
        );
        assert_eq!(input["go"], json!("{{ define \"t\" }}x{{ end }}"));
        assert_eq!(
            harness.script_calls.lock().unwrap()[0]["script"],
            json!("{{ missing.text }}"),
            "literal fields are neither validated nor substituted"
        );
    }

    #[tokio::test]
    async fn text_references_hold_the_complete_output_when_the_preview_is_truncated() {
        let harness = new_harness().await;
        let long = "x".repeat(4096);
        let output = run(
            &harness,
            json!([
                {"id": "big", "read": "fake://echo", "input": {"long": long}},
                {"read": "fake://echo", "input": {"copy": "{{ big.text }}"}, "show": "none"}
            ]),
        )
        .await
        .unwrap();
        assert!(
            output.output().contains("[output truncated]"),
            "{}",
            output.output()
        );
        let copy = recorded(&harness).last().unwrap().1["copy"].clone();
        assert_eq!(
            copy,
            json!(serde_json::to_string(&json!({"long": long})).unwrap())
        );
    }

    #[tokio::test]
    async fn the_operation_limit_skips_remaining_steps_and_dependents() {
        let harness = new_harness().await;
        let items = Value::Array(vec![json!("x"); 32]);
        let output = run(
            &harness,
            json!([
                {"id": "d", "read": "fake://echo", "show": "none", "input": {"items": items}},
                {"for": "item in d.json.items", "max": 32, "read": "fake://echo", "show": "none"},
                {"id": "cut", "for": "item in d.json.items", "max": 32, "read": "fake://echo", "show": "none"},
                {"if": "false", "read": "fake://data"},
                {"read": "fake://echo", "input": {"list": "{{ cut }}"}}
            ]),
        )
        .await
        .unwrap();
        let text = output.output();
        assert!(
            text.contains(
                "*** Result 4 of 5: skipped\nthis call reached the limit of 64 protocol operations"
            ),
            "{text}"
        );
        assert!(
            text.contains("*** Result 5 of 5: skipped\nthis step references step `cut`"),
            "an incomplete loop list is not usable as data: {text}"
        );
        assert_eq!(recorded(&harness).len(), 64);
    }

    #[tokio::test]
    async fn operations_referenced_by_a_condition_stay_in_the_foreground() {
        let harness = new_harness().await;
        let output = run(
            &harness,
            json!([
                {"id": "slow", "exec": "slowpoke://job"},
                {"if": "slow.ok", "read": "fake://data"}
            ]),
        )
        .await
        .unwrap();
        assert!(
            output.output().contains("foreground-done"),
            "{}",
            output.output()
        );
        assert_eq!(*harness.pinned.lock().unwrap(), [true]);
    }
}
