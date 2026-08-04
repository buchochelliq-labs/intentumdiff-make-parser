//! Makefile parser plugin — full-parse mode on tree-sitter-make (issue #48). Review
//! identity lives in rule TARGETS and variable names: rules are labeled by their target
//! list and variable assignments by their name, with recipes/values as semantic children
//! — editing a recipe line pairs under the stable target identity.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    ts_convert::{convert_semantic, node_to_cst},
    tree::SemanticNodeBuilder,
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

const LANGUAGE_ID: &str = "make";
const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

const DEFAULT_OLD: &str = "CC = gcc\n\nbuild: main.c\n\t$(CC) -O2 -o app main.c\n";
const DEFAULT_NEW: &str =
    "CC = gcc\n\nbuild: main.c\n\t$(CC) -O3 -o app main.c\n\nclean:\n\trm -f app\n";

// Rules, assignments, recipes and directives carry review meaning; operators,
// colons and comments are dropped (not listed, no semantic children).
const SEMANTIC_TYPES: &[&str] = &[
    "makefile",
    "rule",
    "targets",
    "prerequisites",
    "recipe",
    "recipe_line",
    "variable_assignment",
    "shell_assignment",
    "define_directive",
    "include_directive",
    "export_directive",
    "conditional",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or(path)
}

fn detect_language_impl(filename: &str, _content: &str) -> String {
    let name = basename(filename).to_lowercase();
    if name == "makefile"
        || name == "gnumakefile"
        || name.ends_with(".mk")
        || name.ends_with(".make")
    {
        LANGUAGE_ID.to_string()
    } else {
        String::new()
    }
}

/// All non-empty LEAF text under `node`, joined — a target list like `build test` keeps
/// both names in the label (CstNode only carries text on leaves).
fn joined_leaf_text(node: &CstNode) -> Option<String> {
    fn collect(node: &CstNode, out: &mut Vec<String>) {
        if node.is_leaf() {
            let text = node.text_or_empty().trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
            return;
        }
        for child in &node.children {
            collect(child, out);
        }
    }
    let mut parts = Vec::new();
    collect(node, &mut parts);
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" ").chars().take(120).collect())
}

/// First descendant of `key_type`, read via its leaves.
fn key_text(node: &CstNode, key_type: &str) -> Option<String> {
    fn find_key(node: &CstNode, key_type: &str) -> Option<String> {
        if node.node_type == key_type {
            return joined_leaf_text(node);
        }
        for child in &node.children {
            if let Some(text) = find_key(child, key_type) {
                return Some(text);
            }
        }
        None
    }
    find_key(node, key_type)
}

/// The node's SOURCE span, whitespace-compacted — recipe lines interleave shell text
/// with variable references, and node_to_cst only keeps text on leaves, so the honest
/// label comes from the source itself.
fn span_text(node: &CstNode, source: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let start = node.start_line as usize;
    let end = node.end_line as usize;
    if start >= lines.len() {
        return None;
    }
    let raw = if start == end {
        let line: Vec<char> = lines[start].chars().collect();
        let from = (node.start_col as usize).min(line.len());
        let to = (node.end_col as usize).min(line.len());
        line[from..to.max(from)].iter().collect::<String>()
    } else {
        lines[start..=(end.min(lines.len() - 1))].join(" ")
    };
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(compact.chars().take(120).collect())
    }
}

fn label_for(node: &CstNode, source: &str) -> String {
    if node.is_leaf() {
        return node.text_or_empty().trim().chars().take(120).collect();
    }
    match node.node_type.as_str() {
        // A rule is identified by its target list ("build", ".PHONY clean", ...).
        "rule" => key_text(node, "targets").unwrap_or_else(|| node.node_type.clone()),
        // Assignments are identified by the variable name (the word before the operator).
        "variable_assignment" | "shell_assignment" => {
            key_text(node, "word").unwrap_or_else(|| node.node_type.clone())
        }
        "targets" | "prerequisites" => {
            joined_leaf_text(node).unwrap_or_else(|| node.node_type.clone())
        }
        // Recipe lines interleave raw shell text with variable references — the leaves
        // alone lose the shell text, so the label is the source span.
        "recipe_line" => span_text(node, source)
            .or_else(|| joined_leaf_text(node))
            .unwrap_or_else(|| node.node_type.clone()),
        _ => node.node_type.clone(),
    }
}

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_make::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load make grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "tree-sitter failed to parse Makefile".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let cst = match parse_source(source) {
        Ok(cst) => cst,
        Err(err) => return format!(r#"{{"error":"{}"}}"#, err),
    };
    let mut memo = std::collections::HashMap::new();
    // Span labeling needs the source; the SDK converter takes label_for as a closure, so
    // capture it here instead of threading a parameter (issue #47).
    let node = convert_semantic(&cst, "0", &mut memo, &is_semantic, &|n| label_for(n, source))
        .unwrap_or_else(|| {
        SemanticNodeBuilder::new("0", "makefile", LANGUAGE_ID, 0, 0, 0, 0, "0").build()
    });
    match serde_json::to_string(&node) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

struct MakeParser;

impl Guest for MakeParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        LANGUAGE_ID.to_string()
    }

    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: DEFAULT_OLD.to_string(),
            new: DEFAULT_NEW.to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        vec![]
    }

    fn language_ids() -> Vec<String> {
        vec![LANGUAGE_ID.to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        5
    }
}

export!(MakeParser);

#[cfg(test)]
mod tests {
    use super::*;
    use intentumdiff_plugin_sdk::tree::SemanticNode;

    fn labels_by_type(node: &SemanticNode, node_type: &str, out: &mut Vec<String>) {
        if node.node_type == node_type {
            out.push(node.label.clone());
        }
        for child in &node.children {
            labels_by_type(child, node_type, out);
        }
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert_eq!(MakeParser::get_parser_mode(), ParserMode::FullParse);
    }

    #[test]
    fn detects_makefiles() {
        assert_eq!(detect_language_impl("Makefile", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl("sub/GNUmakefile", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl("build.mk", ""), LANGUAGE_ID);
        assert_eq!(detect_language_impl("main.rs", ""), "");
    }

    #[test]
    fn rules_and_assignments_are_labeled_by_identity() {
        let parsed = process_impl(DEFAULT_NEW);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut rules = Vec::new();
        labels_by_type(&root, "rule", &mut rules);
        assert_eq!(
            rules,
            vec!["build".to_string(), "clean".to_string()],
            "rules: {rules:?}"
        );
        let mut assigns = Vec::new();
        labels_by_type(&root, "variable_assignment", &mut assigns);
        assert_eq!(assigns, vec!["CC".to_string()], "assigns: {assigns:?}");
    }

    #[test]
    fn recipe_edit_changes_the_root_hash() {
        let old: SemanticNode = serde_json::from_str(&process_impl(DEFAULT_OLD)).unwrap();
        let new: SemanticNode = serde_json::from_str(&process_impl(DEFAULT_NEW)).unwrap();
        assert_ne!(old.structural_hash, new.structural_hash);
    }
}
