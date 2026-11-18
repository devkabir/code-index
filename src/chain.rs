use ignore::WalkBuilder;
use petgraph::graph::DiGraph;
use petgraph::visit::Bfs;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};

#[derive(Clone, Debug)]
pub struct MethodSnippet {
    pub name: String,
    pub class_name: Option<String>,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
    pub calls: Vec<String>,
}

fn last_segment(name: &str) -> &str {
    name.rsplit('\\').next().unwrap_or(name)
}

pub struct CodebaseChainScanner {
    methods: BTreeMap<String, MethodSnippet>,
    graph: DiGraph<String, ()>,
    node_indices: HashMap<String, petgraph::graph::NodeIndex>,
}

impl CodebaseChainScanner {
    pub fn new() -> Self {
        Self {
            methods: BTreeMap::new(),
            graph: DiGraph::new(),
            node_indices: HashMap::new(),
        }
    }

    pub fn scan_dir(&mut self, root: &Path) {
        let walker = WalkBuilder::new(root)
            .standard_filters(false)
            .add_custom_ignore_filename(".csignore")
            .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | ".code-index" | "target")))
            .build();

        for entry in walker.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("php") {
                self.index_php_file(path);
            }
        }

        self.build_graph();
    }

    fn index_php_file(&mut self, path: &Path) {
        let Ok(source) = fs::read_to_string(path) else { return };
        let mut parser = Parser::new();
        let language = tree_sitter_php::language();
        if parser.set_language(language).is_err() {
            return;
        }

        let Some(tree) = parser.parse(&source, None) else { return };

        // Context: one query covers class, interface, trait, enum and global function declarations; method name and body come from node fields.
        let query_str = r#"
            [
                (class_declaration
                    name: (name) @class_name
                    body: (declaration_list
                        (method_declaration) @method
                    )
                )
                (interface_declaration
                    name: (name) @class_name
                    body: (declaration_list
                        (method_declaration) @method
                    )
                )
                (trait_declaration
                    name: (name) @class_name
                    body: (declaration_list
                        (method_declaration) @method
                    )
                )
                (enum_declaration
                    name: (name) @class_name
                    body: (enum_declaration_list
                        (method_declaration) @method
                    )
                )
                (function_definition) @method
            ]
        "#;

        let Ok(query) = Query::new(language, query_str) else { return };
        let mut cursor = QueryCursor::new();
        let matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

        let class_idx = query.capture_index_for_name("class_name");
        let method_idx = query.capture_index_for_name("method");

        for m in matches {
            let mut class_name = None;
            let mut method_node: Option<Node> = None;

            for cap in m.captures {
                if Some(cap.index) == class_idx {
                    class_name = cap.node.utf8_text(source.as_bytes()).ok().map(|s| s.to_string());
                } else if Some(cap.index) == method_idx {
                    method_node = Some(cap.node);
                }
            }

            let Some(node) = method_node else { continue };
            let Some(name_node) = node.child_by_field_name("name") else { continue };
            let Ok(method_name) = name_node.utf8_text(source.as_bytes()) else { continue };
            let calls = node
                .child_by_field_name("body")
                .map(|body| self.extract_calls(body, &source, language, class_name.as_deref()))
                .unwrap_or_default();

            // Context: keys are Class::method, or the bare name for global functions.
            let key = match &class_name {
                Some(c) => format!("{c}::{method_name}"),
                None => method_name.to_string(),
            };

            let snippet = MethodSnippet {
                name: method_name.to_string(),
                class_name,
                file_path: path.to_path_buf(),
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                code: node.utf8_text(source.as_bytes()).unwrap_or("").to_string(),
                calls,
            };

            self.methods.insert(key, snippet);
        }
    }

    fn extract_calls(&self, body_node: Node, source: &str, language: tree_sitter::Language, class_name: Option<&str>) -> Vec<String> {
        let call_query_str = r#"
            [
                (member_call_expression object: (_) @member_object name: (_) @member_name)
                (scoped_call_expression scope: (_) @scope name: (_) @scoped_name)
                (function_call_expression function: (_) @function_name)
            ]
        "#;
        let Ok(call_query) = Query::new(language, call_query_str) else { return vec![] };
        let mut cursor = QueryCursor::new();
        let matches = cursor.matches(&call_query, body_node, source.as_bytes());

        let object_idx = call_query.capture_index_for_name("member_object");
        let member_idx = call_query.capture_index_for_name("member_name");
        let scope_idx = call_query.capture_index_for_name("scope");
        let scoped_idx = call_query.capture_index_for_name("scoped_name");
        let function_idx = call_query.capture_index_for_name("function_name");

        let mut calls = Vec::new();
        for m in matches {
            let (mut object, mut member) = (None, None);
            let (mut scope, mut scoped) = (None, None);
            let mut function = None;

            for cap in m.captures {
                let text = cap.node.utf8_text(source.as_bytes()).ok();
                if Some(cap.index) == object_idx {
                    object = text;
                } else if Some(cap.index) == member_idx {
                    member = text;
                } else if Some(cap.index) == scope_idx {
                    scope = text;
                } else if Some(cap.index) == scoped_idx {
                    scoped = text;
                } else if Some(cap.index) == function_idx {
                    function = text;
                }
            }

            // Context: only $this-> and self::/static:: bind to a known class; other receivers resolve to no edge instead of a bare method name.
            if let (Some(object), Some(name)) = (object, member) {
                if object == "$this" {
                    if let Some(class) = class_name {
                        calls.push(format!("{class}::{name}"));
                    }
                }
            } else if let (Some(scope), Some(name)) = (scope, scoped) {
                let scope = last_segment(scope.trim_start_matches('\\'));
                let target = match scope.to_ascii_lowercase().as_str() {
                    "self" | "static" => class_name.map(|c| c.to_string()),
                    "parent" => None,
                    _ => Some(scope.to_string()),
                };
                if let Some(class) = target {
                    calls.push(format!("{class}::{name}"));
                }
            } else if let Some(name) = function {
                let name = last_segment(name.trim_start_matches('\\'));
                if !name.is_empty() && !name.starts_with('$') {
                    calls.push(name.to_string());
                }
            }
        }
        calls
    }

    fn build_graph(&mut self) {
        for key in self.methods.keys() {
            let idx = self.graph.add_node(key.clone());
            self.node_indices.insert(key.clone(), idx);
        }

        for (caller, snippet) in &self.methods {
            if let Some(&caller_idx) = self.node_indices.get(caller) {
                for callee in &snippet.calls {
                    if let Some(&callee_idx) = self.node_indices.get(callee) {
                        self.graph.add_edge(caller_idx, callee_idx, ());
                    }
                }
            }
        }
    }

    // Context: a bare start name resolves only when exactly one Class::method matches.
    fn resolve(&self, name: &str) -> Option<String> {
        if self.methods.contains_key(name) {
            return Some(name.to_string());
        }
        let suffix = format!("::{name}");
        let mut hits = self.methods.keys().filter(|k| k.ends_with(&suffix));
        match (hits.next(), hits.next()) {
            (Some(key), None) => Some(key.to_string()),
            _ => None,
        }
    }

    pub fn dump_chain(&self, start_method: &str) -> String {
        let Some(start_key) = self.resolve(start_method) else {
            return format!("ERROR: Method '{}' not found or ambiguous; use Class::method.\n", start_method);
        };
        let Some(&start_idx) = self.node_indices.get(&start_key) else {
            return format!("ERROR: Method '{}' not found or ambiguous; use Class::method.\n", start_method);
        };

        let mut bfs = Bfs::new(&self.graph, start_idx);
        let mut out = String::new();
        let mut step = 1;

        while let Some(node_idx) = bfs.next(&self.graph) {
            let symbol_key = &self.graph[node_idx];
            if let Some(snippet) = self.methods.get(symbol_key) {
                out.push_str(&format!(
                    "### [{step}] {symbol_key} ({}:L{}-L{})\n",
                    snippet.file_path.display(),
                    snippet.start_line,
                    snippet.end_line
                ));
                out.push_str("```php\n");
                out.push_str(snippet.code.trim());
                out.push_str("\n```\n\n");
                step += 1;
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner_for(php: &str) -> CodebaseChainScanner {
        let dir = std::env::temp_dir().join(format!("cs-chain-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("sample.php"), php).unwrap();
        let mut scanner = CodebaseChainScanner::new();
        scanner.scan_dir(&dir);
        let _ = fs::remove_dir_all(&dir);
        scanner
    }

    #[test]
    fn parses_declarations_and_scopes_calls() {
        let scanner = scanner_for(
            r#"<?php
function boot() { helper(); }
function helper() {}
class A {
    function run() { $this->save(); self::stop(); other(); Plain::go(); }
    function save() {}
    function stop() {}
}
class B { function save() {} }
trait T { function t() { $this->save(); } }
interface I { function decl(); }
enum E { case X; function e() { boot(); } }
"#,
        );

        for key in ["boot", "helper", "A::run", "A::save", "A::stop", "B::save", "T::t", "I::decl", "E::e"] {
            assert!(!scanner.dump_chain(key).contains("ERROR"), "not indexed: {key}");
        }

        let run = scanner.dump_chain("A::run");
        assert!(run.contains("A::save") && run.contains("A::stop"), "{run}");
        assert!(!run.contains("B::save"), "call leaked across classes: {run}");

        let boot = scanner.dump_chain("boot");
        assert!(boot.contains("helper"), "{boot}");
        assert!(!boot.contains("A::"), "function call matched a method: {boot}");

        assert!(!scanner.dump_chain("T::t").contains("A::save"), "trait $this call leaked");
        assert!(scanner.dump_chain("E::e").contains("boot"));
        assert!(scanner.dump_chain("run").contains("A::run"));
        assert!(scanner.dump_chain("save").contains("ambiguous"));
    }
}
