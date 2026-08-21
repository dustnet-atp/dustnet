use crate::scanner::{Attribute, AttributeValue, Token};

use super::{
    Diagnostic, DiagnosticLevel, ParserAllocationSite, mark_allocation_failure, reject_allocation,
    try_clone_token, try_copy_string, try_diagnostic, try_push,
};

/// Maximum component definitions per document.
const MAX_DEFS: usize = 32;
/// Maximum usages per component.
const MAX_USAGES_PER: usize = 64;
/// Maximum total usages across all components.
const MAX_TOTAL_USAGES: usize = 256;
/// Hard ceiling on the number of tokens produced by expansion. Because a
/// component body may itself contain component usages, expansion is
/// potentially geometric across passes (the "billion laughs" / entity-
/// expansion attack). This bound halts expansion the moment output crosses
/// it, before the tokens are ever allocated into the final stream.
const MAX_EXPANDED_TOKENS: usize = 100_000;

/// A registered component definition.
#[derive(Debug, Clone)]
struct ComponentDef {
    name: String,
    /// Attribute names with optional defaults: `("color", Some("white"))`.
    attrs: Vec<(String, Option<String>)>,
    /// Slot names.
    slots: Vec<String>,
    /// The raw tokens of the definition body (between [def] and [/def]).
    body_tokens: Vec<Token>,
}

/// Built-in element names that components cannot shadow.
const BUILTIN_ELEMENTS: &[&str] = &[
    "page",
    "meta",
    "style",
    "box",
    "row",
    "col",
    "hr",
    "divider",
    "spacer",
    "header",
    "body",
    "footer",
    "nav",
    "text",
    "pre",
    "heading",
    "list",
    "item",
    "link",
    "input",
    "select",
    "option",
    "button",
    "form",
    "art",
    "table",
    "thead",
    "tbody",
    "tr",
    "td",
    "th",
    "animate",
    "frame",
    "element",
    "tween",
    "at",
    "text-animate",
    "live",
    "panel",
    "state",
    "pagination",
    "def",
    "slot",
    "slot-content",
    "br",
];

/// Expand components in a token stream.
///
/// 1. First pass: extract `[def]` blocks into a registry
/// 2. Second pass: replace component usages with expanded tokens
///
/// Returns the expanded token stream and any diagnostics.
pub fn expand_components(tokens: Vec<Token>) -> (Vec<Token>, Vec<Diagnostic>, bool) {
    let mut diagnostics = Vec::new();
    let mut allocation_failed = false;

    // First pass: extract definitions
    let (defs, remaining_tokens) =
        extract_definitions(tokens, &mut diagnostics, &mut allocation_failed);

    if defs.is_empty() {
        return (remaining_tokens, diagnostics, allocation_failed);
    }

    // Second pass: expand usages
    let expanded = expand_usages(
        remaining_tokens,
        &defs,
        &mut diagnostics,
        &mut allocation_failed,
    );

    (expanded, diagnostics, allocation_failed)
}

/// Extract [def] blocks from the token stream.
/// Returns (definitions, remaining tokens with defs removed).
fn extract_definitions(
    tokens: Vec<Token>,
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) -> (Vec<ComponentDef>, Vec<Token>) {
    let mut defs: Vec<ComponentDef> = Vec::new();
    let mut remaining = Vec::new();
    let mut i = 0;

    while let Some(current) = tokens.get(i) {
        match current {
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } if name == "def" => {
                if *self_closing {
                    try_diagnostic(
                        diagnostics,
                        DiagnosticLevel::Warning,
                        "W001",
                        format_args!("[def] cannot be self-closing"),
                        allocation_failed,
                    );
                    i += 1;
                    continue;
                }

                // Parse the def attributes
                let mut comp_name = String::new();
                let mut attrs_str = String::new();
                let mut slots_str = String::new();

                for attr in attributes {
                    match attr.name.as_str() {
                        "name" => {
                            comp_name =
                                try_copy_string(attr_value_str(&attr.value), allocation_failed)
                        }
                        "attrs" => {
                            attrs_str =
                                try_copy_string(attr_value_str(&attr.value), allocation_failed)
                        }
                        "slots" => {
                            slots_str =
                                try_copy_string(attr_value_str(&attr.value), allocation_failed)
                        }
                        _ => {}
                    }
                }

                if comp_name.is_empty() {
                    try_diagnostic(
                        diagnostics,
                        DiagnosticLevel::Error,
                        "E011",
                        format_args!("[def] requires name attribute"),
                        allocation_failed,
                    );
                    i += 1;
                    // Skip until [/def]
                    while let Some(current) = tokens.get(i) {
                        if matches!(current, Token::CloseTag { name } if name == "def") {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }

                // Check for conflicts
                if BUILTIN_ELEMENTS.contains(&comp_name.as_str()) {
                    try_diagnostic(
                        diagnostics,
                        DiagnosticLevel::Error,
                        "E037",
                        format_args!(
                            "component name \"{comp_name}\" conflicts with built-in element"
                        ),
                        allocation_failed,
                    );
                }

                // Check for duplicates
                if defs.iter().any(|d| d.name == comp_name) {
                    try_diagnostic(
                        diagnostics,
                        DiagnosticLevel::Error,
                        "E036",
                        format_args!("duplicate component name \"{comp_name}\""),
                        allocation_failed,
                    );
                }

                // Check max defs
                if defs.len() >= MAX_DEFS {
                    try_diagnostic(
                        diagnostics,
                        DiagnosticLevel::Error,
                        "E038",
                        format_args!("maximum component definitions exceeded ({MAX_DEFS})"),
                        allocation_failed,
                    );
                }

                // Parse attrs with defaults
                let attrs = parse_def_attrs(&attrs_str, allocation_failed);
                let mut slots = Vec::new();
                for slot in slots_str
                    .split(',')
                    .map(str::trim)
                    .filter(|slot| !slot.is_empty())
                {
                    let slot = try_copy_string(slot, allocation_failed);
                    try_push(&mut slots, slot, allocation_failed);
                }

                // Collect body tokens until [/def]
                i += 1; // skip the [def] tag
                let mut body_tokens = Vec::new();
                let mut def_depth = 1u32;

                while let Some(current) = tokens.get(i) {
                    match current {
                        Token::CloseTag { name } if name == "def" => {
                            def_depth -= 1;
                            if def_depth == 0 {
                                i += 1;
                                break;
                            }
                            let token = try_clone_token(current, allocation_failed);
                            try_push(&mut body_tokens, token, allocation_failed);
                        }
                        Token::OpenTag { name, .. } if name == "def" => {
                            def_depth += 1;
                            let token = try_clone_token(current, allocation_failed);
                            try_push(&mut body_tokens, token, allocation_failed);
                        }
                        _ => {
                            let token = try_clone_token(current, allocation_failed);
                            try_push(&mut body_tokens, token, allocation_failed);
                        }
                    }
                    i += 1;
                }

                // Check body doesn't reference other components or itself
                for token in &body_tokens {
                    if let Token::OpenTag { name, .. } = token {
                        if name == &comp_name {
                            try_diagnostic(
                                diagnostics,
                                DiagnosticLevel::Error,
                                "E031",
                                format_args!(
                                    "component \"{comp_name}\" references itself (recursive)"
                                ),
                                allocation_failed,
                            );
                        }
                        // Check for references to other defined components
                        if defs.iter().any(|d| &d.name == name) {
                            try_diagnostic(
                                diagnostics,
                                DiagnosticLevel::Error,
                                "E030",
                                format_args!(
                                    "component \"{comp_name}\" references another component \"{name}\""
                                ),
                                allocation_failed,
                            );
                        }
                    }
                }

                try_push(
                    &mut defs,
                    ComponentDef {
                        name: comp_name,
                        attrs,
                        slots,
                        body_tokens,
                    },
                    allocation_failed,
                );
            }
            _ => {
                let token = try_clone_token(current, allocation_failed);
                try_push(&mut remaining, token, allocation_failed);
                i += 1;
            }
        }
    }

    (defs, remaining)
}

/// Expand component usages in a token stream.
/// Runs iteratively until no more component tags remain.
fn expand_usages(
    tokens: Vec<Token>,
    defs: &[ComponentDef],
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) -> Vec<Token> {
    let mut tokens = tokens;
    // Iterate expansion until stable (components in slot content get expanded)
    for _ in 0..4 {
        let has_component = tokens.iter().any(|t| match t {
            Token::OpenTag { name, .. } => defs.iter().any(|d| d.name == *name),
            _ => false,
        });
        if !has_component {
            break;
        }
        // Stop before another multiplying pass if we are already at the
        // ceiling; `expand_usages_once` also halts mid-pass, this bounds
        // accumulation across the four passes.
        if tokens.len() > MAX_EXPANDED_TOKENS {
            break;
        }
        tokens = expand_usages_once(tokens, defs, diagnostics, allocation_failed);
    }
    tokens
}

fn expand_usages_once(
    tokens: Vec<Token>,
    defs: &[ComponentDef],
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) -> Vec<Token> {
    let mut usage_counts = Vec::new();
    if usage_counts.try_reserve_exact(defs.len()).is_err() {
        mark_allocation_failure(allocation_failed);
        return Vec::new();
    }
    usage_counts.resize(defs.len(), 0usize);
    let mut total_usages = 0usize;
    let mut output = Vec::new();
    let mut i = 0;

    while let Some(current) = tokens.get(i) {
        match current {
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } => {
                // Check if this tag is a component usage
                let def_idx = defs.iter().position(|d| d.name == *name);

                if let Some(idx) = def_idx {
                    // Component usage — expand it
                    total_usages += 1;
                    // `idx` came from `defs.iter().position` and `usage_counts`
                    // is sized to `defs`, so the slot always resolves.
                    let usages = match usage_counts.get_mut(idx) {
                        Some(count) => {
                            *count += 1;
                            *count
                        }
                        None => 0,
                    };

                    if usages > MAX_USAGES_PER {
                        try_diagnostic(
                            diagnostics,
                            DiagnosticLevel::Error,
                            "E039",
                            format_args!(
                                "max usages for component \"{}\" exceeded ({MAX_USAGES_PER})",
                                name
                            ),
                            allocation_failed,
                        );
                        // Halt expansion: the document is already rejected, and
                        // continuing would let the abusive component keep growing
                        // the token stream.
                        return output;
                    }

                    if total_usages > MAX_TOTAL_USAGES {
                        try_diagnostic(
                            diagnostics,
                            DiagnosticLevel::Error,
                            "E040",
                            format_args!(
                                "max total component usages exceeded ({MAX_TOTAL_USAGES})"
                            ),
                            allocation_failed,
                        );
                        return output;
                    }

                    // Same index, same reason it resolves; taken through
                    // `get` so the expander has no panic on remote input.
                    let Some(def) = defs.get(idx) else {
                        return output;
                    };

                    // Build attribute map from usage
                    let attr_map = build_attr_map(def, attributes, diagnostics, allocation_failed);

                    // Collect slot content from children (if not self-closing)
                    let slot_content = if *self_closing {
                        i += 1;
                        std::collections::HashMap::new()
                    } else {
                        i += 1;
                        collect_slot_content(&tokens, &mut i, name, &def.slots, allocation_failed)
                    };

                    // Expand: substitute attributes and slots in body
                    let expanded = expand_body(
                        &def.body_tokens,
                        &attr_map,
                        &slot_content,
                        allocation_failed,
                    );

                    if output.len() + expanded.len() > MAX_EXPANDED_TOKENS {
                        try_diagnostic(
                            diagnostics,
                            DiagnosticLevel::Error,
                            "E041",
                            format_args!(
                                "expanded component output exceeds token limit ({MAX_EXPANDED_TOKENS})"
                            ),
                            allocation_failed,
                        );
                        return output;
                    }

                    if output.try_reserve(expanded.len()).is_err() {
                        mark_allocation_failure(allocation_failed);
                        return output;
                    }
                    output.extend(expanded);
                } else {
                    // Not a component — pass through
                    let token = try_clone_token(current, allocation_failed);
                    try_push(&mut output, token, allocation_failed);
                    i += 1;
                }
            }
            Token::CloseTag { name } => {
                // Check if closing a component tag — skip it (already consumed)
                let is_component = defs.iter().any(|d| d.name == *name);
                if !is_component {
                    let token = try_clone_token(current, allocation_failed);
                    try_push(&mut output, token, allocation_failed);
                }
                i += 1;
            }
            _ => {
                let token = try_clone_token(current, allocation_failed);
                try_push(&mut output, token, allocation_failed);
                i += 1;
            }
        }
    }

    output
}

/// Build an attribute value map from a component usage, validating against the definition.
fn build_attr_map(
    def: &ComponentDef,
    usage_attrs: &[Attribute],
    diagnostics: &mut Vec<Diagnostic>,
    allocation_failed: &mut bool,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if reject_allocation(ParserAllocationSite::ComponentMap)
        || map.try_reserve(def.attrs.len()).is_err()
    {
        mark_allocation_failure(allocation_failed);
        return map;
    }

    // Start with defaults
    for (name, default) in &def.attrs {
        if let Some(val) = default {
            map.insert(
                try_copy_string(name, allocation_failed),
                try_copy_string(val, allocation_failed),
            );
        }
    }

    // Override with provided values
    for attr in usage_attrs {
        let known = def.attrs.iter().any(|(n, _)| *n == attr.name);
        if known {
            map.insert(
                try_copy_string(&attr.name, allocation_failed),
                try_copy_string(attr_value_str(&attr.value), allocation_failed),
            );
        } else {
            try_diagnostic(
                diagnostics,
                DiagnosticLevel::Error,
                "E034",
                format_args!(
                    "unknown attribute \"{}\" on component [{}]",
                    attr.name, def.name
                ),
                allocation_failed,
            );
        }
    }

    // Check required attrs (no default) are provided
    for (name, default) in &def.attrs {
        if default.is_none() && !map.contains_key(name) {
            try_diagnostic(
                diagnostics,
                DiagnosticLevel::Error,
                "E033",
                format_args!(
                    "required attribute \"{name}\" missing on component [{}]",
                    def.name
                ),
                allocation_failed,
            );
        }
    }

    map
}

/// Collect slot content from caller's children until [/component-name].
/// Returns a map of slot_name → token stream.
fn collect_slot_content(
    tokens: &[Token],
    i: &mut usize,
    close_tag: &str,
    slot_names: &[String],
    allocation_failed: &mut bool,
) -> std::collections::HashMap<String, Vec<Token>> {
    let mut slots: std::collections::HashMap<String, Vec<Token>> = std::collections::HashMap::new();
    if reject_allocation(ParserAllocationSite::SlotMap)
        || slots.try_reserve(slot_names.len()).is_err()
    {
        mark_allocation_failure(allocation_failed);
        return slots;
    }
    let mut default_content: Vec<Token> = Vec::new();

    while let Some(current) = tokens.get(*i) {
        match current {
            Token::CloseTag { name } if name == close_tag => {
                *i += 1;
                break;
            }
            // [slot-content name="..."] ... [/slot-content]
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } if name == "slot-content" => {
                let slot_name = attributes
                    .iter()
                    .find(|a| a.name == "name")
                    .map(|a| attr_value_str(&a.value))
                    .unwrap_or_default();

                *i += 1;

                if *self_closing {
                    continue;
                }

                // Collect tokens until [/slot-content]
                let mut content = Vec::new();
                let mut depth = 1u32;
                while let Some(current) = tokens.get(*i) {
                    match current {
                        Token::CloseTag { name } if name == "slot-content" => {
                            depth -= 1;
                            if depth == 0 {
                                *i += 1;
                                break;
                            }
                            let token = try_clone_token(current, allocation_failed);
                            try_push(&mut content, token, allocation_failed);
                        }
                        Token::OpenTag { name, .. } if name == "slot-content" => {
                            depth += 1;
                            let token = try_clone_token(current, allocation_failed);
                            try_push(&mut content, token, allocation_failed);
                        }
                        _ => {
                            let token = try_clone_token(current, allocation_failed);
                            try_push(&mut content, token, allocation_failed);
                        }
                    }
                    *i += 1;
                }

                let slot_name = try_copy_string(slot_name, allocation_failed);
                if slots.contains_key(slot_name.as_str()) {
                    slots.insert(slot_name, content);
                } else if reject_allocation(ParserAllocationSite::SlotMap)
                    || slots.try_reserve(1).is_err()
                {
                    mark_allocation_failure(allocation_failed);
                } else {
                    slots.insert(slot_name, content);
                }
            }
            _ => {
                // Content not wrapped in [slot-content] goes to the first/only slot
                let token = try_clone_token(current, allocation_failed);
                try_push(&mut default_content, token, allocation_failed);
                *i += 1;
            }
        }
    }

    // If there's default content and a single slot, assign it
    if !default_content.is_empty() {
        // Trim whitespace-only text tokens from the default content
        let has_meaningful = default_content.iter().any(|t| match t {
            Token::Text(s) => !s.trim().is_empty(),
            _ => true,
        });

        if has_meaningful {
            if slot_names.len() == 1 {
                let slot_name = try_copy_string(
                    slot_names.first().map_or("", String::as_str),
                    allocation_failed,
                );
                if !slots.contains_key(slot_name.as_str()) {
                    if reject_allocation(ParserAllocationSite::SlotMap)
                        || slots.try_reserve(1).is_err()
                    {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        slots.insert(slot_name, default_content);
                    }
                }
            } else if !slot_names.is_empty() {
                // Multiple slots but no [slot-content] — assign to first slot
                let slot_name = try_copy_string(
                    slot_names.first().map_or("", String::as_str),
                    allocation_failed,
                );
                if !slots.contains_key(slot_name.as_str()) {
                    if reject_allocation(ParserAllocationSite::SlotMap)
                        || slots.try_reserve(1).is_err()
                    {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        slots.insert(slot_name, default_content);
                    }
                }
            }
        }
    }

    slots
}

/// Expand a component body by substituting $attr and [slot] references.
fn expand_body(
    body: &[Token],
    attrs: &std::collections::HashMap<String, String>,
    slot_content: &std::collections::HashMap<String, Vec<Token>>,
    allocation_failed: &mut bool,
) -> Vec<Token> {
    let mut output = Vec::new();

    let mut i = 0;
    while let Some(current) = body.get(i) {
        match current {
            // [slot name="..."] or [slot name="..." /] — insert slot content
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } if name == "slot" => {
                let slot_name = attributes
                    .iter()
                    .find(|a| a.name == "name")
                    .map(|a| attr_value_str(&a.value))
                    .unwrap_or_default();

                if let Some(content) = slot_content.get(slot_name) {
                    for token in content {
                        let token = try_clone_token(token, allocation_failed);
                        try_push(&mut output, token, allocation_failed);
                    }
                } else if !*self_closing {
                    // Use default slot content (between [slot] and [/slot])
                    i += 1;
                    let mut default = Vec::new();
                    let mut depth = 1u32;
                    while let Some(current) = body.get(i) {
                        match current {
                            Token::CloseTag { name } if name == "slot" => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                                let token = try_clone_token(current, allocation_failed);
                                try_push(&mut default, token, allocation_failed);
                            }
                            Token::OpenTag { name, .. } if name == "slot" => {
                                depth += 1;
                                let token = try_clone_token(current, allocation_failed);
                                try_push(&mut default, token, allocation_failed);
                            }
                            _ => {
                                let token = try_clone_token(current, allocation_failed);
                                try_push(&mut default, token, allocation_failed);
                            }
                        }
                        i += 1;
                    }
                    if output.try_reserve(default.len()).is_err() {
                        mark_allocation_failure(allocation_failed);
                    } else {
                        output.extend(default);
                    }
                }
                // Skip [/slot] if not self-closing
                i += 1;
                continue;
            }

            // Close tag for [slot] — skip (already handled above)
            Token::CloseTag { name } if name == "slot" => {
                i += 1;
                continue;
            }

            // Text tokens — substitute $attr references
            Token::Text(text) => {
                let text = substitute_attrs(text, attrs, allocation_failed);
                try_push(&mut output, Token::Text(text), allocation_failed);
                i += 1;
            }

            // Open tags — substitute $attr in attribute values
            Token::OpenTag {
                name,
                attributes,
                self_closing,
            } => {
                let mut new_attrs = Vec::new();
                if new_attrs.try_reserve_exact(attributes.len()).is_err() {
                    mark_allocation_failure(allocation_failed);
                } else {
                    for attr in attributes {
                        new_attrs.push(Attribute {
                            name: try_copy_string(&attr.name, allocation_failed),
                            value: substitute_attr_value(&attr.value, attrs, allocation_failed),
                        });
                    }
                }

                let name = try_copy_string(name, allocation_failed);
                try_push(
                    &mut output,
                    Token::OpenTag {
                        name,
                        attributes: new_attrs,
                        self_closing: *self_closing,
                    },
                    allocation_failed,
                );
                i += 1;
            }

            other => {
                let token = try_clone_token(other, allocation_failed);
                try_push(&mut output, token, allocation_failed);
                i += 1;
            }
        }
    }

    output
}

/// Substitute $attr references in a text string.
/// `$$` produces a literal `$`.
fn substitute_attrs(
    text: &str,
    attrs: &std::collections::HashMap<String, String>,
    allocation_failed: &mut bool,
) -> String {
    let mut result = String::new();

    let mut cursor = 0usize;
    while let Some(relative) = text[cursor..].find('$') {
        let dollar = cursor + relative;
        if !try_append_substitution(&mut result, &text[cursor..dollar], allocation_failed) {
            return result;
        }
        let after = dollar + 1;
        if text[after..].starts_with('$') {
            if !try_append_substitution(&mut result, "$", allocation_failed) {
                return result;
            }
            cursor = after + 1;
            continue;
        }

        let mut end = after;
        for (offset, character) in text[after..].char_indices() {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                end = after + offset + character.len_utf8();
            } else {
                break;
            }
        }
        let name = &text[after..end];
        if let Some(value) = attrs.get(name) {
            if !try_append_substitution(&mut result, value, allocation_failed) {
                return result;
            }
        } else {
            if !try_append_substitution(&mut result, "$", allocation_failed)
                || !try_append_substitution(&mut result, name, allocation_failed)
            {
                return result;
            }
        }
        cursor = end;
    }
    let _ = try_append_substitution(&mut result, &text[cursor..], allocation_failed);
    result
}

fn try_append_substitution(output: &mut String, value: &str, allocation_failed: &mut bool) -> bool {
    if value.is_empty() {
        return true;
    }
    if reject_allocation(ParserAllocationSite::Substitution)
        || output.try_reserve(value.len()).is_err()
    {
        mark_allocation_failure(allocation_failed);
        return false;
    }
    output.push_str(value);
    true
}

/// Substitute $attr in an attribute value.
fn substitute_attr_value(
    value: &AttributeValue,
    attrs: &std::collections::HashMap<String, String>,
    allocation_failed: &mut bool,
) -> AttributeValue {
    match value {
        AttributeValue::String(s) => {
            AttributeValue::String(substitute_attrs(s, attrs, allocation_failed))
        }
        AttributeValue::Ident(s) => {
            let substituted = substitute_attrs(s, attrs, allocation_failed);
            AttributeValue::Ident(substituted)
        }
        AttributeValue::Flag => AttributeValue::Flag,
    }
}

/// Parse the `attrs` string from [def]: "name1,name2=default,name3".
fn parse_def_attrs(attrs_str: &str, allocation_failed: &mut bool) -> Vec<(String, Option<String>)> {
    if attrs_str.is_empty() {
        return Vec::new();
    }

    let mut attrs = Vec::new();
    for attr in attrs_str
        .split(',')
        .map(str::trim)
        .filter(|attr| !attr.is_empty())
    {
        let parsed = if let Some((name, default)) = attr.split_once('=') {
            (
                try_copy_string(name.trim(), allocation_failed),
                Some(try_copy_string(default.trim(), allocation_failed)),
            )
        } else {
            (try_copy_string(attr, allocation_failed), None)
        };
        try_push(&mut attrs, parsed, allocation_failed);
    }
    attrs
}

fn attr_value_str(val: &AttributeValue) -> &str {
    match val {
        AttributeValue::String(s) | AttributeValue::Ident(s) => s,
        AttributeValue::Flag => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::Scanner;

    fn scan(input: &str) -> Vec<Token> {
        Scanner::new(input.as_bytes()).unwrap().scan_all().unwrap()
    }

    fn expand(input: &str) -> (Vec<Token>, Vec<Diagnostic>) {
        let (tokens, diagnostics, resource_exhausted) = expand_components(scan(input));
        assert!(!resource_exhausted);
        (tokens, diagnostics)
    }

    fn expand_and_check(input: &str) -> Vec<Token> {
        let (tokens, diags) = expand(input);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.level == DiagnosticLevel::Error)
            .collect();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        tokens
    }

    fn has_error(diags: &[Diagnostic], code: &str) -> bool {
        diags
            .iter()
            .any(|d| d.code == code && d.level == DiagnosticLevel::Error)
    }

    fn tokens_contain_text(tokens: &[Token], needle: &str) -> bool {
        tokens.iter().any(|t| match t {
            Token::Text(s) => s.contains(needle),
            _ => false,
        })
    }

    fn tokens_contain_tag(tokens: &[Token], tag: &str) -> bool {
        tokens.iter().any(|t| match t {
            Token::OpenTag { name, .. } => name == tag,
            _ => false,
        })
    }

    // ─── Basic Expansion ─────────────────────────────────────

    #[test]
    fn basic_component() {
        let tokens = expand_and_check(
            r#"[def name="greeting" attrs="who"]
                [text]Hello $who[/text]
            [/def]
            [greeting who="world" /]"#,
        );
        assert!(tokens_contain_text(&tokens, "Hello world"));
        assert!(!tokens_contain_tag(&tokens, "def"));
        assert!(!tokens_contain_tag(&tokens, "greeting"));
    }

    #[test]
    fn component_with_defaults() {
        let tokens = expand_and_check(
            r#"[def name="badge" attrs="color=white,label"]
                [text fg=$color]$label[/text]
            [/def]
            [badge label="OK" /]"#,
        );
        // Should have fg=white (default) and text "OK"
        let has_white = tokens.iter().any(|t| match t {
            Token::OpenTag { attributes, .. } => attributes.iter().any(|a| {
                a.name == "fg" && matches!(&a.value, AttributeValue::Ident(s) if s == "white")
            }),
            _ => false,
        });
        assert!(has_white, "should use default color=white");
        assert!(tokens_contain_text(&tokens, "OK"));
    }

    #[test]
    fn component_override_default() {
        let tokens = expand_and_check(
            r#"[def name="badge" attrs="color=white"]
                [text fg=$color]X[/text]
            [/def]
            [badge color=red /]"#,
        );
        let has_red = tokens.iter().any(|t| match t {
            Token::OpenTag { attributes, .. } => attributes.iter().any(|a| {
                a.name == "fg" && matches!(&a.value, AttributeValue::Ident(s) if s == "red")
            }),
            _ => false,
        });
        assert!(has_red, "should override default with red");
    }

    #[test]
    fn dollar_escape() {
        let tokens = expand_and_check(
            r#"[def name="price" attrs="amount"]
                [text]$$USD $amount[/text]
            [/def]
            [price amount="49.99" /]"#,
        );
        assert!(tokens_contain_text(&tokens, "$USD 49.99"));
    }

    // ─── Slots ───────────────────────────────────────────────

    #[test]
    fn single_slot() {
        let tokens = expand_and_check(
            r#"[def name="wrapper" slots="content"]
                [box][slot name="content" /][/box]
            [/def]
            [wrapper][text]Inside[/text][/wrapper]"#,
        );
        assert!(tokens_contain_tag(&tokens, "box"));
        assert!(tokens_contain_text(&tokens, "Inside"));
    }

    #[test]
    fn named_slots() {
        let tokens = expand_and_check(
            r#"[def name="dialog" attrs="title" slots="body,actions"]
                [box title=$title]
                    [slot name="body" /]
                    [hr /]
                    [slot name="actions" /]
                [/box]
            [/def]
            [dialog title="Confirm"]
                [slot-content name="body"][text]Are you sure?[/text][/slot-content]
                [slot-content name="actions"][button action=submit]Yes[/button][/slot-content]
            [/dialog]"#,
        );
        assert!(tokens_contain_text(&tokens, "Are you sure?"));
        assert!(tokens_contain_tag(&tokens, "button"));
        assert!(tokens_contain_tag(&tokens, "hr"));
    }

    #[test]
    fn undeclared_slot_names_grow_only_through_fallible_reservation() {
        let tokens = expand_and_check(
            r#"[def name="card" slots="body"][slot name="body" /][/def]
            [card]
              [slot-content name="one"][text]1[/text][/slot-content]
              [slot-content name="two"][text]2[/text][/slot-content]
              [slot-content name="three"][text]3[/text][/slot-content]
              [slot-content name="body"][text]declared[/text][/slot-content]
            [/card]"#,
        );
        assert!(tokens_contain_text(&tokens, "declared"));
    }

    #[test]
    fn default_slot_content() {
        let tokens = expand_and_check(
            r#"[def name="section" slots="content,footer"]
                [slot name="content" /]
                [slot name="footer"][hr /][/slot]
            [/def]
            [section][text]Body here[/text][/section]"#,
        );
        assert!(tokens_contain_text(&tokens, "Body here"));
        // Footer should use default [hr]
        assert!(tokens_contain_tag(&tokens, "hr"));
    }

    // ─── Multiple Usages ─────────────────────────────────────

    #[test]
    fn multiple_usages() {
        let tokens = expand_and_check(
            r#"[def name="tag" attrs="label"]
                [text bold]$label[/text]
            [/def]
            [tag label="one" /]
            [tag label="two" /]
            [tag label="three" /]"#,
        );
        assert!(tokens_contain_text(&tokens, "one"));
        assert!(tokens_contain_text(&tokens, "two"));
        assert!(tokens_contain_text(&tokens, "three"));
    }

    // ─── Validation Errors ───────────────────────────────────

    #[test]
    fn recursive_component_e031() {
        let (_, diags) = expand(r#"[def name="loop"][loop /][/def]"#);
        assert!(has_error(&diags, "E031"));
    }

    #[test]
    fn component_references_another_e030() {
        let (_, diags) = expand(
            r#"[def name="inner"][text]x[/text][/def]
            [def name="outer"][inner /][/def]"#,
        );
        assert!(has_error(&diags, "E030"));
    }

    #[test]
    fn duplicate_name_e036() {
        let (_, diags) = expand(
            r#"[def name="x"][/def]
            [def name="x"][/def]"#,
        );
        assert!(has_error(&diags, "E036"));
    }

    #[test]
    fn builtin_conflict_e037() {
        let (_, diags) = expand(r#"[def name="text"][/def]"#);
        assert!(has_error(&diags, "E037"));
    }

    #[test]
    fn missing_required_attr_e033() {
        let (_, diags) = expand(
            r#"[def name="badge" attrs="color,label"][text]$label[/text][/def]
            [badge color=red /]"#,
        );
        assert!(has_error(&diags, "E033"));
    }

    #[test]
    fn unknown_attr_e034() {
        let (_, diags) = expand(
            r#"[def name="badge" attrs="color"][text]x[/text][/def]
            [badge color=red size=big /]"#,
        );
        assert!(has_error(&diags, "E034"));
    }

    // ─── No Components ───────────────────────────────────────

    #[test]
    fn no_defs_passthrough() {
        let input = "[page mode=document][text]hello[/text][/page]";
        let (tokens, diags) = expand(input);
        assert!(diags.is_empty());
        // Should be unchanged
        assert!(tokens_contain_text(&tokens, "hello"));
        assert!(tokens_contain_tag(&tokens, "page"));
    }

    // ─── Substitution Edge Cases ─────────────────────────────

    #[test]
    fn attr_in_tag_attribute() {
        let tokens = expand_and_check(
            r#"[def name="colorbox" attrs="color"]
                [box fg=$color][text]colored[/text][/box]
            [/def]
            [colorbox color=cyan /]"#,
        );
        let has_cyan = tokens.iter().any(|t| match t {
            Token::OpenTag {
                name, attributes, ..
            } => {
                name == "box"
                    && attributes.iter().any(|a| {
                        a.name == "fg"
                            && matches!(&a.value, AttributeValue::Ident(s) if s == "cyan")
                    })
            }
            _ => false,
        });
        assert!(has_cyan);
    }

    #[test]
    fn self_closing_component() {
        let tokens = expand_and_check(
            r#"[def name="dot" attrs="color"]
                [text fg=$color].[/text]
            [/def]
            [dot color=red /]"#,
        );
        assert!(tokens_contain_text(&tokens, "."));
    }

    #[test]
    fn component_with_panel() {
        let tokens = expand_and_check(
            r#"[def name="toggle" attrs="id,label" slots="content"]
                [panel id=$id state="off"]
                    [state name="off"][text]OFF[/text][/state]
                    [state name="on"][slot name="content" /][/state]
                [/panel]
            [/def]
            [toggle id="t1" label="Test"]
                [text]Enabled![/text]
            [/toggle]"#,
        );
        assert!(tokens_contain_tag(&tokens, "panel"));
        assert!(tokens_contain_tag(&tokens, "state"));
        assert!(tokens_contain_text(&tokens, "Enabled!"));
    }

    // ─── Expansion limits (DoS resistance) ───────────────────

    /// Exceeding a usage cap must *halt* expansion, not merely emit a
    /// diagnostic while continuing to grow the stream. Regression test for the
    /// entity-expansion ("billion laughs") class of DoS. Here every usage is
    /// the same component, so the per-component cap (E039) is what fires.
    #[test]
    fn excessive_usages_halts_expansion() {
        let mut input = String::from(r#"[def name="x"][text]hi[/text][/def]"#);
        for _ in 0..400 {
            input.push_str("[x /]");
        }
        let (tokens, diags) = expand(&input);

        assert!(
            has_error(&diags, "E039") || has_error(&diags, "E040"),
            "usage overflow must be reported"
        );
        // Because expansion stops at the cap rather than running all 400
        // usages, far fewer than 400 bodies reach the output.
        let expansions = tokens
            .iter()
            .filter(|t| matches!(t, Token::Text(s) if s.contains("hi")))
            .count();
        assert!(
            expansions <= MAX_USAGES_PER,
            "expansion should halt at the per-component cap, got {expansions} bodies"
        );
    }

    /// A component body may itself contain component usages, so expansion is
    /// potentially geometric across passes. The output-token ceiling must keep
    /// even a deliberately-multiplying document bounded.
    #[test]
    fn geometric_expansion_stays_bounded() {
        // Each level embeds many copies of the next, so naive expansion would
        // multiply without limit across passes.
        let input = r#"
            [def name="d"][text]x[/text][text]x[/text][text]x[/text][/def]
            [def name="c"][d /][d /][d /][d /][d /][d /][d /][d /][/def]
            [def name="b"][c /][c /][c /][c /][c /][c /][c /][c /][/def]
            [def name="a"][b /][b /][b /][b /][b /][b /][b /][b /][/def]
            [a /][a /][a /][a /][a /][a /][a /][a /]
        "#;
        let (tokens, _diags) = expand(input);
        assert!(
            tokens.len() <= MAX_EXPANDED_TOKENS + 64,
            "expansion must stay within the token ceiling, got {}",
            tokens.len()
        );
    }
}
