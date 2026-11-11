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

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

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
    "unexport_directive",
    "override_directive",
    "private_directive",
    "conditional",
    // Synthetic leaves (see `attach_flavor_tokens`): the assignment operator decides
    // expansion semantics (`=` deferred, `:=`/`::=` immediate, `:::=` immediate with
    // escaped re-expansion, `?=` conditional, `+=` append, `!=` shell) and the
    // directive keyword decides scope (`export`, `override`, `private`, `unexport`).
    "assignment_operator",
    "directive_keyword",
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
        // The name is a DIRECT child: a target-specific `all: X = 1` also has a target
        // `list` whose words must not be taken for the name.
        "variable_assignment" | "shell_assignment" => node
            .children
            .iter()
            .find(|c| c.node_type == "word")
            .and_then(joined_leaf_text)
            .or_else(|| key_text(node, "word"))
            .unwrap_or_else(|| node.node_type.clone()),
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

/// GNU make 4.4's `:::=` operator is unknown to tree-sitter-make (it parses as a `::`
/// rule plus an ERROR). For PARSING only, an assignment's `:::=` is rewritten to the
/// same-length `::= ` so the line is recognised as an assignment; every byte offset is
/// unchanged, all CST text is still read from the ORIGINAL source, and the rewritten
/// offsets are returned so the operator token is restored to `:::=` (issue #4).
///
/// Only an operator that can head an assignment is rewritten: the line is not a recipe
/// line (no leading tab), and no `=` or `#` precedes the `:::=` on that line.
fn mask_triple_colon_assignments(source: &str) -> (String, Vec<usize>) {
    let mut masked = source.as_bytes().to_vec();
    let mut offsets = Vec::new();
    let mut line_start = 0usize;
    for line in source.split_inclusive('\n') {
        if !line.starts_with('\t') {
            if let Some(at) = line.find(":::=") {
                let head = &line[..at];
                if !head.contains('=') && !head.contains('#') {
                    let abs = line_start + at;
                    masked[abs..abs + 4].copy_from_slice(b"::= ");
                    offsets.push(abs);
                }
            }
        }
        line_start += line.len();
    }
    // Only ASCII bytes were swapped for ASCII bytes, so the result stays valid UTF-8.
    (String::from_utf8(masked).unwrap_or_else(|_| source.to_string()), offsets)
}

fn token_leaf(node_type: &str, text: String, node: tree_sitter::Node<'_>, extra_cols: u32) -> CstNode {
    CstNode {
        node_type: node_type.to_string(),
        named: true,
        text: Some(text),
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32 + extra_cols,
        children: Vec::new(),
    }
}

/// `node_to_cst` keeps named children only, so the anonymous assignment operator and
/// directive keyword vanished: `X = $(Y)` and `X := $(Y)` (deferred vs immediate
/// expansion) produced identical trees and hashes and were certified as style (issue #4).
/// Walk the tree-sitter tree in parallel with the CST (whose children are exactly the
/// named children, in order) and insert those tokens as leaves, in source order.
///
/// The same walk also normalises value payloads and drops comments
/// (see `payload_leaf_text`).
fn attach_flavor_tokens(ts: tree_sitter::Node<'_>, cst: &mut CstNode, source: &str, masked: &[usize]) {
    if let Some(text) = payload_leaf_text(ts, source) {
        cst.text = Some(text);
        cst.children.clear();
        return;
    }
    let children = std::mem::take(&mut cst.children);
    let mut kept = Vec::with_capacity(children.len());
    let mut originals = children.into_iter();
    for i in 0..ts.child_count() {
        let Some(child) = ts.child(i) else { continue };
        if !child.is_named() {
            continue;
        }
        let Some(mut target) = originals.next() else { break };
        if child.kind() == "comment" {
            continue;
        }
        attach_flavor_tokens(child, &mut target, source, masked);
        kept.push(target);
    }
    cst.children = kept;

    let mut tokens = Vec::new();
    match ts.kind() {
        "variable_assignment" | "shell_assignment" | "VPATH_assignment"
        | "RECIPEPREFIX_assignment" | "define_directive" => {
            if let Some(op) = ts.child_by_field_name("operator") {
                let (text, extra) = if masked.contains(&op.start_byte()) {
                    (":::=".to_string(), 1)
                } else {
                    (source.get(op.byte_range()).unwrap_or("").to_string(), 0)
                };
                let mut leaf = token_leaf("assignment_operator", text, op, extra);
                reattach_glued_flavor(cst, &mut leaf);
                tokens.push(leaf);
            }
        }
        "export_directive" | "unexport_directive" | "override_directive"
        | "private_directive" => {
            if let Some(kw) = ts.child(0).filter(|c| !c.is_named()) {
                let text = source.get(kw.byte_range()).unwrap_or("").to_string();
                tokens.push(token_leaf("directive_keyword", text, kw, 0));
            }
        }
        _ => {}
    }
    if tokens.is_empty() {
        return;
    }
    // A node that had no named children was a leaf carrying its text; it now has
    // children, so drop the text as a childful node would have none.
    if cst.children.is_empty() {
        cst.text = None;
    }
    for token in tokens {
        let at = cst
            .children
            .iter()
            .position(|c| (c.start_line, c.start_col) > (token.start_line, token.start_col))
            .unwrap_or(cst.children.len());
        cst.children.insert(at, token);
    }
}

/// tree-sitter-make's `word` token admits `?` and `+`, so an unspaced `X?=a` / `X+=a`
/// lexes as the name `X?` / `X+` with a plain `=`. GNU make reads those as the `?=`
/// (conditional) and `+=` (append) flavors of `X`; move the glued character from the
/// name onto the operator so the spaced and unspaced spellings agree.
fn reattach_glued_flavor(cst: &mut CstNode, op: &mut CstNode) {
    if op.text.as_deref() != Some("=") {
        return;
    }
    let Some(name) = cst.children.iter_mut().rev().find(|c| {
        c.node_type == "word" && c.end_line == op.start_line && c.end_col == op.start_col
    }) else {
        return;
    };
    let Some(word) = name.text.clone() else { return };
    let Some(flavor) = word.chars().last().filter(|c| *c == '?' || *c == '+') else {
        return;
    };
    if word.len() < 2 || name.start_line != name.end_line {
        return;
    }
    name.text = Some(word[..word.len() - 1].to_string());
    name.end_col -= 1;
    op.text = Some(format!("{flavor}="));
    op.start_col -= 1;
}

/// Value payloads (`text`, `shell_command`) interleave unnamed literal text with named
/// `variable_reference` children; `node_to_cst` keeps text on leaves only, so in
/// `X = $(Y) b` the literal ` b` vanished from labels and hashes. Such a node becomes a
/// leaf carrying its whitespace-compacted source span (backslash-newline continuations
/// collapse to one space, as make does). Comments carry no meaning and are dropped.
///
/// The payload is NOT truncated: it is what the structural hash covers, so a cap let an
/// edit past it (e.g. a `$(Y)` -> `$(Z)` after 4096 characters) hash identically (#6).
/// Only the display label is bounded, by `label_for`.
fn payload_leaf_text(ts: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    if !matches!(ts.kind(), "text" | "shell_command") || ts.named_child_count() == 0 {
        return None;
    }
    let raw = source.get(ts.byte_range()).unwrap_or("");
    let joined = raw.replace("\\\r\n", " ").replace("\\\n", " ");
    Some(joined.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_make::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load make grammar".to_string())?;
    let (parse_text, masked) = mask_triple_colon_assignments(source);
    let tree = parser
        .parse(&parse_text, None)
        .ok_or_else(|| "tree-sitter failed to parse Makefile".to_string())?;
    // Text comes from the ORIGINAL bytes; masking kept every offset identical.
    let mut cst = node_to_cst(tree.root_node(), source.as_bytes());
    attach_flavor_tokens(tree.root_node(), &mut cst, source, &masked);
    Ok(cst)
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

    // ---- issue #4: assignment flavor and directive keywords are semantic tokens ----

    fn tree(source: &str) -> SemanticNode {
        let parsed = process_impl(source);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        serde_json::from_str(&parsed).unwrap()
    }

    fn find_all<'a>(node: &'a SemanticNode, node_type: &str, out: &mut Vec<&'a SemanticNode>) {
        if node.node_type == node_type {
            out.push(node);
        }
        for child in &node.children {
            find_all(child, node_type, out);
        }
    }

    fn operators(source: &str) -> Vec<String> {
        let mut out = Vec::new();
        labels_by_type(&tree(source), "assignment_operator", &mut out);
        out
    }

    fn span(node: &SemanticNode) -> (u32, u32, u32, u32) {
        let p = &node.position;
        (p.start_line, p.start_col, p.end_line, p.end_col)
    }

    const FLAVOR_OLD: &str = "Y = before\nX = $(Y)\nY = after\n.PHONY: all\nall:\n\t@echo $(X)\n";
    const FLAVOR_NEW: &str = "Y = before\nX := $(Y)\nY = after\n.PHONY: all\nall:\n\t@echo $(X)\n";

    #[test]
    fn assessment_pair_deferred_to_immediate_is_a_real_change() {
        let old = tree(FLAVOR_OLD);
        let new = tree(FLAVOR_NEW);
        assert_ne!(old.structural_hash, new.structural_hash);
        let (mut o, mut n) = (Vec::new(), Vec::new());
        find_all(&old, "variable_assignment", &mut o);
        find_all(&new, "variable_assignment", &mut n);
        // Identity (the variable name) is unchanged; only X's payload hash moves.
        let names = |v: &Vec<&SemanticNode>| v.iter().map(|n| n.label.clone()).collect::<Vec<_>>();
        assert_eq!(names(&o), vec!["Y", "X", "Y"]);
        assert_eq!(names(&n), names(&o));
        assert_eq!(o[0].structural_hash, n[0].structural_hash);
        assert_ne!(o[1].structural_hash, n[1].structural_hash);
        assert_eq!(o[2].structural_hash, n[2].structural_hash);
        // The operator is its own token, with its exact source span.
        let (mut oo, mut no) = (Vec::new(), Vec::new());
        find_all(o[1], "assignment_operator", &mut oo);
        find_all(n[1], "assignment_operator", &mut no);
        assert_eq!(oo[0].label, "=");
        assert_eq!(span(oo[0]), (1, 2, 1, 3));
        assert_eq!(no[0].label, ":=");
        assert_eq!(span(no[0]), (1, 2, 1, 4));
    }

    #[test]
    fn issue_pair_emits_distinct_operator_tokens() {
        let old = "X = $(Y)\nY = a\nall:\n\t@echo $(X)\n";
        let new = "X := $(Y)\nY = a\nall:\n\t@echo $(X)\n";
        assert_eq!(operators(old), vec!["=", "="]);
        assert_eq!(operators(new), vec![":=", "="]);
        assert_ne!(tree(old).structural_hash, tree(new).structural_hash);
    }

    #[test]
    fn every_gnu_make_flavor_is_preserved_and_distinct() {
        let flavors = ["=", ":=", "::=", ":::=", "?=", "+=", "!="];
        let mut hashes = Vec::new();
        for op in flavors {
            let src = format!("X {op} ls\nall:\n\t@echo $(X)\n");
            assert_eq!(operators(&src), vec![op.to_string()], "{src:?}");
            let root = tree(&src);
            let mut assigns = Vec::new();
            find_all(&root, "variable_assignment", &mut assigns);
            find_all(&root, "shell_assignment", &mut assigns);
            assert_eq!(assigns.len(), 1, "{op}: {root:?}");
            assert_eq!(assigns[0].label, "X");
            let mut toks = Vec::new();
            find_all(assigns[0], "assignment_operator", &mut toks);
            assert_eq!(span(toks[0]), (0, 2, 0, 2 + op.len() as u32), "{op}");
            hashes.push(root.structural_hash.clone());
        }
        let unique: std::collections::BTreeSet<_> = hashes.iter().collect();
        assert_eq!(unique.len(), flavors.len(), "flavors must not collide");
    }

    #[test]
    fn unspaced_and_spaced_operators_are_the_same_flavor() {
        for op in ["=", ":=", "::=", ":::=", "?=", "+=", "!="] {
            let tight = format!("X{op}ls\n");
            let loose = format!("X  {op}   ls\n");
            assert_eq!(operators(&tight), vec![op.to_string()], "{tight:?}");
            assert_eq!(
                tree(&tight).structural_hash,
                tree(&loose).structural_hash,
                "whitespace around {op} is layout"
            );
        }
    }

    #[test]
    fn triple_colon_keeps_value_text_and_positions() {
        let src = "Y = a\nX :::= $(Y) b\nall:\n\t@echo $(X)\n";
        let root = tree(src);
        let mut assigns = Vec::new();
        find_all(&root, "variable_assignment", &mut assigns);
        assert_eq!(assigns.len(), 2);
        assert_eq!(assigns[1].label, "X");
        assert_eq!(span(assigns[1]).0, 1);
        let mut rules = Vec::new();
        labels_by_type(&root, "rule", &mut rules);
        assert_eq!(rules, vec!["all"], "`X :::=` must not parse as a `::` rule");
        // The value is part of the payload: a value edit under :::= still changes it.
        let edited = tree("Y = a\nX :::= $(Y) c\nall:\n\t@echo $(X)\n");
        assert_ne!(root.structural_hash, edited.structural_hash);
        // `:::=` vs `::=` differ (re-expansion semantics differ).
        let double = tree("Y = a\nX ::= $(Y) b\nall:\n\t@echo $(X)\n");
        assert_ne!(root.structural_hash, double.structural_hash);
    }

    #[test]
    fn triple_colon_inside_recipe_or_value_is_not_rewritten() {
        // In a recipe line, and after an earlier `=`, the bytes stay data.
        let src = "X = a:::=b\nall:\n\techo a:::=b\n";
        assert_eq!(operators(src), vec!["="]);
        let mut recipes = Vec::new();
        labels_by_type(&tree(src), "recipe_line", &mut recipes);
        assert_eq!(recipes, vec!["echo a:::=b"]);
    }

    #[test]
    fn override_export_private_prefixes_are_tokens() {
        let cases = [
            ("override X := a\n", "override_directive", "override"),
            ("export X := a\n", "export_directive", "export"),
            ("private X := a\n", "private_directive", "private"),
        ];
        let plain = tree("X := a\n");
        for (src, wrapper, kw) in cases {
            let root = tree(src);
            let mut wrappers = Vec::new();
            find_all(&root, wrapper, &mut wrappers);
            assert_eq!(wrappers.len(), 1, "{src:?}");
            let mut kws = Vec::new();
            labels_by_type(wrappers[0], "directive_keyword", &mut kws);
            assert_eq!(kws, vec![kw.to_string()]);
            assert_eq!(operators(src), vec![":="]);
            assert_ne!(root.structural_hash, plain.structural_hash, "{src:?}");
        }
        // Prefix combined with each flavor change still moves the hash.
        assert_ne!(
            tree("override X = a\n").structural_hash,
            tree("override X := a\n").structural_hash
        );
        assert_ne!(
            tree("export X ?= a\n").structural_hash,
            tree("export X += a\n").structural_hash
        );
        assert_ne!(
            tree("export X := a\n").structural_hash,
            tree("override X := a\n").structural_hash
        );
        let mut kws = Vec::new();
        labels_by_type(&tree("export X\nunexport Y\n"), "directive_keyword", &mut kws);
        assert_eq!(kws, vec!["export", "unexport"]);
    }

    #[test]
    fn define_and_target_specific_assignments_keep_flavor() {
        assert_eq!(operators("define F :=\nx\nendef\n"), vec![":="]);
        assert_ne!(
            tree("define F =\nx\nendef\n").structural_hash,
            tree("define F :=\nx\nendef\n").structural_hash
        );
        assert_eq!(operators("override define F +=\nx\nendef\n"), vec!["+="]);
        // Target-specific: the name is the variable, not the target.
        let root = tree("all: X := 1\n");
        let mut assigns = Vec::new();
        labels_by_type(&root, "variable_assignment", &mut assigns);
        assert_eq!(assigns, vec!["X"]);
        assert_eq!(operators("all: X := 1\n"), vec![":="]);
        assert_ne!(
            root.structural_hash,
            tree("all: X = 1\n").structural_hash
        );
        assert_eq!(operators("VPATH := src\n"), vec![":="]);
    }

    #[test]
    fn multiline_values_keep_flavor() {
        let old = "X = a \\\n    b\nall:\n\t@echo $(X)\n";
        let new = "X := a \\\n    b\nall:\n\t@echo $(X)\n";
        assert_eq!(operators(old), vec!["="]);
        assert_eq!(operators(new), vec![":="]);
        assert_ne!(tree(old).structural_hash, tree(new).structural_hash);
    }

    #[test]
    fn whitespace_and_comment_only_controls_stay_equal() {
        let base = tree(FLAVOR_OLD);
        let commented = tree(
            "# ordinary comment\nY = before\nX = $(Y)\n\n# another\nY = after\n.PHONY: all\nall:\n\t@echo $(X)\n",
        );
        assert_eq!(base.structural_hash, commented.structural_hash);
        let spaced = tree("Y   =   before\nX=$(Y)\nY = after\n.PHONY: all\nall:\n\t@echo $(X)\n");
        assert_eq!(base.structural_hash, spaced.structural_hash);
        // A quoted '#' inside a recipe is data, not a comment, so editing it is a change.
        let quoted_a = tree("all:\n\t@echo '#a'\n");
        let quoted_b = tree("all:\n\t@echo '#b'\n");
        assert_ne!(quoted_a.structural_hash, quoted_b.structural_hash);
    }

    #[test]
    fn output_is_deterministic() {
        for src in [FLAVOR_OLD, FLAVOR_NEW, "X :::= a\nexport Y != ls\n"] {
            assert_eq!(process_impl(src), process_impl(src));
        }
    }

    #[test]
    fn literal_text_beside_references_is_payload() {
        let old = tree("Y = a\nX = $(Y) b\n");
        let new = tree("Y = a\nX = $(Y) c\n");
        assert_ne!(old.structural_hash, new.structural_hash);
        let old_sh = tree("X != echo $(Y) b\n");
        let new_sh = tree("X != echo $(Y) c\n");
        assert_ne!(old_sh.structural_hash, new_sh.structural_hash);
        // Continuation layout is not payload.
        assert_eq!(
            tree("X = $(Y) \\\n    b\n").structural_hash,
            tree("X = $(Y) b\n").structural_hash
        );
    }

    #[test]
    fn glued_conditional_and_append_keep_the_variable_name() {
        for (src, op) in [("X?=a\n", "?="), ("X+=a\n", "+=")] {
            let mut names = Vec::new();
            labels_by_type(&tree(src), "variable_assignment", &mut names);
            assert_eq!(names, vec!["X"], "{src:?}");
            let root = tree(src);
            let mut toks = Vec::new();
            find_all(&root, "assignment_operator", &mut toks);
            assert_eq!(toks[0].label, op);
            assert_eq!(span(toks[0]), (0, 1, 0, 3));
        }
    }


    // Issue #6: a reference past any display-length limit still reaches the hash.
    #[test]
    fn long_payload_reference_edit_changes_the_hash() {
        let long = "a".repeat(4100);
        let old = tree(&format!("X = {long}$(Y)\n"));
        let new = tree(&format!("X = {long}$(Z)\n"));
        assert_ne!(old.structural_hash, new.structural_hash);
        let mut assignments = (Vec::new(), Vec::new());
        find_all(&old, "variable_assignment", &mut assignments.0);
        find_all(&new, "variable_assignment", &mut assignments.1);
        assert_ne!(assignments.0[0].structural_hash, assignments.1[0].structural_hash);
        // Short assignments are unaffected; labels stay bounded identities.
        assert_eq!(assignments.0[0].label, "X");
        assert_eq!(tree("X = $(Y) b\n").structural_hash, tree("X = $(Y)   b\n").structural_hash);
    }
}
