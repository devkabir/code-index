use ignore::WalkBuilder;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser, Query, QueryCursor};

// Context: pattern order is the pattern_index used in extract_calls; #eq?/#not-eq? are evaluated by QueryCursor::matches in tree-sitter 0.20.10.
const CALL_QUERY: &str = r#"
(member_call_expression object: (_) @target name: (name) @name (#eq? @target "$this"))
(nullsafe_member_call_expression object: (_) @target name: (name) @name (#eq? @target "$this"))
(member_call_expression object: (_) @target name: (name) @name (#not-eq? @target "$this"))
(nullsafe_member_call_expression object: (_) @target name: (name) @name (#not-eq? @target "$this"))
(scoped_call_expression scope: (relative_scope) @target name: (name) @name)
(scoped_call_expression scope: [(name) (qualified_name)] @target name: (name) @name)
(function_call_expression function: [(name) (qualified_name)] @target)
"#;

#[derive(Clone, Debug)]
enum Target {
    Own(String),
    Parent(String),
    Class(String, String),
    Func(Vec<String>),
    Dynamic,
}

#[derive(Clone, Debug)]
struct Call {
    text: String,
    line: usize,
    target: Target,
}

#[derive(Clone, Debug)]
pub struct MethodSnippet {
    pub key: String,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub code: String,
    class: Option<usize>,
    calls: Vec<Call>,
    edges: Vec<usize>,
    notes: Vec<String>,
}

#[derive(Default)]
struct ClassInfo {
    parent: Option<String>,
    traits: Vec<String>,
    // Context: (trait fqn lowercase, method lowercase) removed by `insteadof`.
    excluded: Vec<(String, String)>,
    // Context: (source trait, original method lowercase, alias lowercase) from `as`.
    aliases: Vec<(Option<String>, String, String)>,
    methods: HashMap<String, usize>,
}

// Context: imports reset at every namespace declaration; keys are lowercase aliases, values are FQNs.
#[derive(Default)]
struct Scope {
    ns: String,
    classes: HashMap<String, String>,
    functions: HashMap<String, String>,
}

enum Found {
    Hits(Vec<usize>),
    Conflict(Vec<usize>),
}

fn text<'a>(node: Node, src: &'a str) -> Option<&'a str> {
    (!node.is_missing()).then(|| node.utf8_text(src.as_bytes()).ok()).flatten()
}

fn field<'a>(node: Node, name: &str, src: &'a str) -> Option<&'a str> {
    node.child_by_field_name(name).and_then(|n| text(n, src))
}

fn join(ns: &str, name: &str) -> String {
    if ns.is_empty() { name.to_string() } else { format!("{ns}\\{name}") }
}

fn last_segment(name: &str) -> &str {
    name.rsplit('\\').next().unwrap_or(name)
}

fn uniq(mut ids: Vec<usize>) -> Vec<usize> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

// Context: fully qualified names skip imports; otherwise the first segment goes through the import table, else the namespace is prepended.
fn resolve_class(scope: &Scope, name: &str) -> String {
    if let Some(abs) = name.strip_prefix('\\') {
        return abs.to_string();
    }
    let (first, rest) = name.split_once('\\').map_or((name, None), |(f, r)| (f, Some(r)));
    match scope.classes.get(&first.to_lowercase()) {
        Some(fqn) => rest.map_or(fqn.clone(), |r| format!("{fqn}\\{r}")),
        None => join(&scope.ns, name),
    }
}

// Context: unqualified function calls try `use function`, then the current namespace, then the global function.
fn function_candidates(scope: &Scope, name: &str) -> Vec<String> {
    if let Some(abs) = name.strip_prefix('\\') {
        return vec![abs.to_string()];
    }
    if name.contains('\\') {
        return vec![resolve_class(scope, name)];
    }
    if let Some(fqn) = scope.functions.get(&name.to_lowercase()) {
        return vec![fqn.clone()];
    }
    let mut out = vec![join(&scope.ns, name)];
    if !scope.ns.is_empty() {
        out.push(name.to_string());
    }
    out
}

fn has_keyword(node: Node, keyword: &str) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(|c| !c.is_named() && c.kind().eq_ignore_ascii_case(keyword));
    found
}

fn add_import(scope: &mut Scope, is_function: bool, fqn: &str, alias: Option<&str>) {
    let fqn = fqn.trim_start_matches('\\').to_string();
    let alias = alias.unwrap_or_else(|| last_segment(&fqn)).to_lowercase();
    let table = if is_function { &mut scope.functions } else { &mut scope.classes };
    table.insert(alias, fqn);
}

// Context: covers `use A\B;`, `use A\B as C;`, `use function A\f;`, and group `use A\{B, C as D};`; `use const` is ignored.
fn parse_imports(node: Node, src: &str, scope: &mut Scope) {
    let decl_function = has_keyword(node, "function");
    if has_keyword(node, "const") {
        return;
    }
    let mut prefix = "";
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_name" => prefix = text(child, src).unwrap_or(""),
            "namespace_use_clause" | "namespace_use_group_clause" => {
                let mut inner = child.walk();
                let parts: Vec<Node> = child.named_children(&mut inner).collect();
                let Some(name) = parts.iter().find(|p| p.kind() != "namespace_aliasing_clause").and_then(|p| text(*p, src)) else { continue };
                let alias = parts
                    .iter()
                    .find(|p| p.kind() == "namespace_aliasing_clause")
                    .and_then(|p| p.named_child(0))
                    .and_then(|n| text(n, src));
                if has_keyword(child, "const") {
                    continue;
                }
                let fqn = if prefix.is_empty() { name.to_string() } else { format!("{prefix}\\{name}") };
                add_import(scope, decl_function || has_keyword(child, "function"), &fqn, alias);
            }
            "namespace_use_group" => {
                let mut inner = child.walk();
                let clauses: Vec<Node> = child.named_children(&mut inner).collect();
                for clause in clauses {
                    let mut c = clause.walk();
                    let parts: Vec<Node> = clause.named_children(&mut c).collect();
                    let Some(name) = parts.iter().find(|p| p.kind() == "namespace_name").and_then(|p| text(*p, src)) else { continue };
                    let alias = parts
                        .iter()
                        .find(|p| p.kind() == "namespace_aliasing_clause")
                        .and_then(|p| p.named_child(0))
                        .and_then(|n| text(n, src));
                    if has_keyword(clause, "const") {
                        continue;
                    }
                    let fqn = if prefix.is_empty() { name.to_string() } else { format!("{prefix}\\{name}") };
                    add_import(scope, decl_function || has_keyword(clause, "function"), &fqn, alias);
                }
            }
            _ => {}
        }
    }
}

// Context: `use T1, T2 { T1::m insteadof T2; T2::m as alias; m as protected; }` inside a class body.
fn parse_trait_use(node: Node, src: &str, scope: &Scope, class: &mut ClassInfo) {
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "name" | "qualified_name" => {
                if let Some(t) = text(child, src) {
                    class.traits.push(resolve_class(scope, t));
                }
            }
            "use_list" => {
                let mut inner = child.walk();
                let rules: Vec<Node> = child.named_children(&mut inner).collect();
                for rule in rules {
                    let mut c = rule.walk();
                    let parts: Vec<Node> = rule.named_children(&mut c).collect();
                    let Some(&source) = parts.first() else { continue };
                    let (from, method) = if source.kind() == "class_constant_access_expression" {
                        let from = source.named_child(0).and_then(|n| text(n, src)).map(|t| resolve_class(scope, t));
                        (from, source.named_child(1).and_then(|n| text(n, src)))
                    } else {
                        (None, text(source, src))
                    };
                    let Some(method) = method.map(str::to_lowercase) else { continue };
                    let names = parts[1..].iter().filter(|p| p.kind() == "name").filter_map(|p| text(*p, src));
                    match rule.kind() {
                        "use_instead_of_clause" => {
                            for loser in names {
                                class.excluded.push((resolve_class(scope, loser).to_lowercase(), method.clone()));
                            }
                        }
                        "use_as_clause" => {
                            if let Some(alias) = names.last() {
                                class.aliases.push((from, method, alias.to_lowercase()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn extract_calls(query: &Query, body: Node, src: &str, scope: &Scope) -> Vec<Call> {
    let target_idx = query.capture_index_for_name("target");
    let name_idx = query.capture_index_for_name("name");
    let mut cursor = QueryCursor::new();
    let mut calls = Vec::new();
    for m in cursor.matches(query, body, src.as_bytes()) {
        let get = |idx: Option<u32>| m.captures.iter().find(|c| Some(c.index) == idx).map(|c| c.node);
        let Some(target) = get(target_idx).and_then(|n| text(n, src)) else { continue };
        let name_node = get(name_idx);
        let name = name_node.and_then(|n| text(n, src)).unwrap_or("");
        let line = name_node.or(get(target_idx)).map_or(0, |n| n.start_position().row + 1);
        let lname = name.to_lowercase();
        let (text, target) = match m.pattern_index {
            0 | 1 => (format!("$this->{name}()"), Target::Own(lname)),
            2 | 3 => (format!("{target}->{name}()"), Target::Dynamic),
            4 if target.eq_ignore_ascii_case("parent") => (format!("{target}::{name}()"), Target::Parent(lname)),
            4 => (format!("{target}::{name}()"), Target::Own(lname)),
            5 => (format!("{target}::{name}()"), Target::Class(resolve_class(scope, target), lname)),
            _ => (format!("{target}()"), Target::Func(function_candidates(scope, target))),
        };
        calls.push(Call { text, line, target });
    }
    calls
}

pub struct CodebaseChainScanner {
    methods: Vec<MethodSnippet>,
    classes: Vec<ClassInfo>,
    class_index: HashMap<String, Vec<usize>>,
    function_index: HashMap<String, Vec<usize>>,
    call_query: Query,
}

impl CodebaseChainScanner {
    pub fn new() -> Self {
        Self {
            methods: Vec::new(),
            classes: Vec::new(),
            class_index: HashMap::new(),
            function_index: HashMap::new(),
            call_query: Query::new(tree_sitter_php::language(), CALL_QUERY).expect("valid call query"),
        }
    }

    pub fn scan_dir(&mut self, root: &Path) {
        let mut parser = Parser::new();
        if parser.set_language(tree_sitter_php::language()).is_err() {
            return;
        }
        let walker = WalkBuilder::new(root)
            .standard_filters(false)
            .add_custom_ignore_filename(".csignore")
            .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | ".code-index" | "target")))
            .build();

        for entry in walker.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("php") {
                let Ok(source) = fs::read_to_string(path) else { continue };
                let Some(tree) = parser.parse(&source, None) else { continue };
                self.walk(tree.root_node(), &source, path, &mut Scope::default());
            }
        }

        self.link();
    }

    // Context: one tree walk carries namespace and import state, which query match order cannot.
    fn walk(&mut self, node: Node, src: &str, file: &Path, scope: &mut Scope) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "namespace_definition" => {
                    let ns = field(child, "name", src).unwrap_or("").to_string();
                    match child.child_by_field_name("body") {
                        Some(body) => self.walk(body, src, file, &mut Scope { ns, ..Scope::default() }),
                        None => *scope = Scope { ns, ..Scope::default() },
                    }
                }
                "namespace_use_declaration" => parse_imports(child, src, scope),
                "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" => {
                    self.index_class(child, src, file, scope)
                }
                "function_definition" => {
                    let Some(name) = field(child, "name", src) else { continue };
                    let fqn = join(&scope.ns, name);
                    let id = self.add_method(fqn.clone(), None, child, src, file, scope);
                    self.function_index.entry(fqn.to_lowercase()).or_default().push(id);
                }
                _ => self.walk(child, src, file, scope),
            }
        }
    }

    fn index_class(&mut self, node: Node, src: &str, file: &Path, scope: &Scope) {
        let (Some(name), Some(body)) = (field(node, "name", src), node.child_by_field_name("body")) else { return };
        let fqn = join(&scope.ns, name);
        let mut class = ClassInfo::default();
        let mut cursor = node.walk();
        class.parent = node
            .named_children(&mut cursor)
            .find(|c| c.kind() == "base_clause")
            .and_then(|b| b.named_child(0))
            .and_then(|n| text(n, src))
            .map(|t| resolve_class(scope, t));

        let members: Vec<Node> = body.named_children(&mut cursor).collect();
        for member in &members {
            if member.kind() == "use_declaration" {
                parse_trait_use(*member, src, scope, &mut class);
            }
        }

        let idx = self.classes.len();
        self.classes.push(class);
        self.class_index.entry(fqn.to_lowercase()).or_default().push(idx);
        for member in members.into_iter().filter(|m| m.kind() == "method_declaration") {
            let Some(method) = field(member, "name", src) else { continue };
            let id = self.add_method(format!("{fqn}::{method}"), Some(idx), member, src, file, scope);
            self.classes[idx].methods.insert(method.to_lowercase(), id);
        }
    }

    fn add_method(&mut self, key: String, class: Option<usize>, node: Node, src: &str, file: &Path, scope: &Scope) -> usize {
        let calls = node
            .child_by_field_name("body")
            .map(|body| extract_calls(&self.call_query, body, src, scope))
            .unwrap_or_default();
        self.methods.push(MethodSnippet {
            key,
            file_path: file.to_path_buf(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            code: node.utf8_text(src.as_bytes()).unwrap_or("").to_string(),
            class,
            calls,
            edges: Vec::new(),
            notes: Vec::new(),
        });
        self.methods.len() - 1
    }

    fn link(&mut self) {
        for id in 0..self.methods.len() {
            let (mut edges, mut notes) = (Vec::new(), Vec::new());
            let method = &self.methods[id];
            for call in &method.calls {
                match self.resolve_call(method.class, &call.target) {
                    Ok(target) if !edges.contains(&target) => edges.push(target),
                    Ok(_) => {}
                    Err(reason) => notes.push(format!("{}:L{} `{}`: {reason}", method.file_path.display(), call.line, call.text)),
                }
            }
            self.methods[id].edges = edges;
            self.methods[id].notes = notes;
        }
    }

    fn resolve_call(&self, class: Option<usize>, target: &Target) -> Result<usize, String> {
        let class = class.map(|c| &self.classes[c]);
        let found = match target {
            Target::Own(m) => match class {
                Some(c) => self.find_in(c, m, 0),
                None => return Err("unresolved: no enclosing class".into()),
            },
            Target::Parent(m) => match class.and_then(|c| c.parent.as_deref()) {
                Some(parent) => self.find(parent, m, 0),
                None => return Err("unresolved: no parent class".into()),
            },
            Target::Class(c, m) => self.find(c, m, 0),
            Target::Func(candidates) => Found::Hits(
                candidates
                    .iter()
                    .find_map(|c| self.function_index.get(&c.to_lowercase()))
                    .cloned()
                    .unwrap_or_default(),
            ),
            Target::Dynamic => return Err("unresolved: receiver type unknown".into()),
        };
        match found {
            Found::Hits(ids) if ids.len() == 1 => Ok(ids[0]),
            Found::Hits(ids) if ids.is_empty() => Err("unresolved: no matching definition".into()),
            Found::Hits(ids) => Err(format!("ambiguous: {}", self.locate_all(&ids))),
            Found::Conflict(ids) => Err(format!("trait conflict: {}", self.locate_all(&ids))),
        }
    }

    // Context: every definition of the class FQN is searched, so duplicates across files surface as ambiguous.
    fn find(&self, class: &str, method: &str, depth: u8) -> Found {
        let mut hits = Vec::new();
        if depth < 32 {
            for &c in self.class_index.get(&class.to_lowercase()).into_iter().flatten() {
                match self.find_in(&self.classes[c], method, depth + 1) {
                    Found::Hits(h) => hits.extend(h),
                    conflict => return conflict,
                }
            }
        }
        Found::Hits(uniq(hits))
    }

    // Context: PHP precedence is own method, then trait (after insteadof/as), then parent.
    fn find_in(&self, class: &ClassInfo, method: &str, depth: u8) -> Found {
        if let Some(&id) = class.methods.get(method) {
            return Found::Hits(vec![id]);
        }
        let mut hits = Vec::new();
        for (from, original, alias) in &class.aliases {
            if alias == method {
                let sources: Vec<&String> = from.iter().collect::<Vec<_>>();
                let sources = if sources.is_empty() { class.traits.iter().collect() } else { sources };
                for t in sources {
                    if let Found::Hits(h) = self.find(t, original, depth) {
                        hits.extend(h);
                    }
                }
            }
        }
        if hits.is_empty() {
            for t in &class.traits {
                if class.excluded.iter().any(|(et, em)| *et == t.to_lowercase() && em == method) {
                    continue;
                }
                match self.find(t, method, depth) {
                    Found::Hits(h) => hits.extend(h),
                    conflict => return conflict,
                }
            }
        }
        let hits = uniq(hits);
        match hits.len() {
            0 => class.parent.as_deref().map_or(Found::Hits(hits), |p| self.find(p, method, depth)),
            1 => Found::Hits(hits),
            _ => Found::Conflict(hits),
        }
    }

    fn locate(&self, id: usize) -> String {
        let m = &self.methods[id];
        format!("{} ({}:L{})", m.key, m.file_path.display(), m.start_line)
    }

    fn locate_all(&self, ids: &[usize]) -> String {
        ids.iter().map(|&id| self.locate(id)).collect::<Vec<_>>().join(", ")
    }

    // Context: exact FQN (case-insensitive, optional leading `\`) wins; otherwise a namespace-suffix or bare method-name match.
    fn lookup(&self, query: &str) -> Vec<usize> {
        let q = query.trim_start_matches('\\').to_lowercase();
        let keys: Vec<String> = self.methods.iter().map(|m| m.key.to_lowercase()).collect();
        let exact: Vec<usize> = (0..keys.len()).filter(|&i| keys[i] == q).collect();
        if !exact.is_empty() {
            return exact;
        }
        let (ns_suffix, method_suffix) = (format!("\\{q}"), format!("::{q}"));
        (0..keys.len()).filter(|&i| keys[i].ends_with(&ns_suffix) || keys[i].ends_with(&method_suffix)).collect()
    }

    pub fn dump_chain(&self, start_method: &str) -> String {
        let start = match self.lookup(start_method).as_slice() {
            [] => return format!("ERROR: Method '{start_method}' not found.\n"),
            [id] => *id,
            ids => {
                let list: String = ids.iter().map(|&id| format!("- {}\n", self.locate(id))).collect();
                return format!("ERROR: Method '{start_method}' is ambiguous; use one of:\n{list}");
            }
        };

        let mut seen = vec![false; self.methods.len()];
        let mut queue = VecDeque::from([start]);
        seen[start] = true;
        let (mut out, mut notes, mut step) = (String::new(), Vec::new(), 1);

        while let Some(id) = queue.pop_front() {
            let m = &self.methods[id];
            out.push_str(&format!("### [{step}] {} ({}:L{}-L{})\n", m.key, m.file_path.display(), m.start_line, m.end_line));
            out.push_str("```php\n");
            out.push_str(m.code.trim());
            out.push_str("\n```\n\n");
            step += 1;
            notes.extend(m.notes.iter());
            for &next in &m.edges {
                if !seen[next] {
                    seen[next] = true;
                    queue.push_back(next);
                }
            }
        }

        if !notes.is_empty() {
            out.push_str("### Unresolved calls\n");
            for note in notes {
                out.push_str(&format!("- {note}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(name: &str, files: &[(&str, &str)]) -> CodebaseChainScanner {
        let dir = std::env::temp_dir().join(format!("cs-chain-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (file, php) in files {
            fs::write(dir.join(file), php).unwrap();
        }
        let mut scanner = CodebaseChainScanner::new();
        scanner.scan_dir(&dir);
        let _ = fs::remove_dir_all(&dir);
        scanner
    }

    fn steps(out: &str) -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.strip_prefix("### ["))
            .filter_map(|l| l.split_once("] "))
            .filter_map(|(_, rest)| rest.split(" (").next())
            .collect()
    }

    fn notes(out: &str) -> Vec<&str> {
        out.lines().filter(|l| l.starts_with("- ")).collect()
    }

    #[test]
    fn parses_declarations_and_scopes_calls() {
        let scanner = scan(
            "basic",
            &[(
                "sample.php",
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
            )],
        );

        for key in ["boot", "helper", "A::run", "A::save", "A::stop", "B::save", "T::t", "I::decl", "E::e"] {
            assert!(!scanner.dump_chain(key).contains("ERROR"), "not indexed: {key}");
        }

        let run = scanner.dump_chain("A::run");
        assert_eq!(steps(&run), ["A::run", "A::save", "A::stop"], "{run}");
        assert!(run.contains("`other()`: unresolved") && run.contains("`Plain::go()`: unresolved"), "{run}");

        assert_eq!(steps(&scanner.dump_chain("boot")), ["boot", "helper"]);
        assert_eq!(steps(&scanner.dump_chain("T::t")), ["T::t"], "trait $this call leaked");
        assert_eq!(steps(&scanner.dump_chain("E::e")), ["E::e", "boot", "helper"]);
        assert_eq!(steps(&scanner.dump_chain("run")), ["A::run", "A::save", "A::stop"]);
        assert!(scanner.dump_chain("save").contains("ambiguous"));
        assert!(scanner.dump_chain("nope").contains("not found"));
    }

    #[test]
    fn resolves_namespaces_and_imports() {
        let scanner = scan(
            "ns",
            &[
                (
                    "a.php",
                    r#"<?php
namespace App\Models;
class User { function save() { helper(); \strlen('x'); } }
function helper() {}
"#,
                ),
                (
                    "b.php",
                    r#"<?php
namespace App\Http;
use App\Models\User;
use App\{Models\User as U2};
use function App\Models\helper as h;
class Controller {
    function run() { User::save(); U2::SAVE(); h(); \App\Models\helper(); Models\Nope::x(); }
}
"#,
                ),
                (
                    "c.php",
                    r#"<?php
namespace One { class A { function a() {} } }
namespace Two { use One\A; class B { function b() { A::a(); } } }
namespace Three { class C { function c() { A::a(); } } }
"#,
                ),
            ],
        );

        let run = scanner.dump_chain("App\\Http\\Controller::run");
        assert_eq!(steps(&run), ["App\\Http\\Controller::run", "App\\Models\\User::save", "App\\Models\\helper"], "{run}");
        let n = notes(&run);
        assert_eq!(n.len(), 2, "{run}");
        assert!(n.iter().any(|l| l.contains("b.php:L7 `Models\\Nope::x()`: unresolved")), "{run}");
        assert!(n.iter().any(|l| l.contains("a.php:L3 `\\strlen()`: unresolved")), "{run}");

        assert_eq!(steps(&scanner.dump_chain("\\app\\models\\user::save")), ["App\\Models\\User::save", "App\\Models\\helper"]);
        assert_eq!(steps(&scanner.dump_chain("User::save"))[0], "App\\Models\\User::save");
        assert_eq!(steps(&scanner.dump_chain("Two\\B::b")), ["Two\\B::b", "One\\A::a"]);
        let c = scanner.dump_chain("Three\\C::c");
        assert_eq!(steps(&c), ["Three\\C::c"], "import leaked across namespace blocks: {c}");
        assert!(c.contains("`A::a()`: unresolved"), "{c}");
    }

    #[test]
    fn reports_duplicates_as_ambiguous() {
        let scanner = scan(
            "dup",
            &[
                ("one.php", "<?php\nnamespace App\\One;\nclass Svc { function go() {} }\nclass Dup { function go() {} }\n"),
                ("two.php", "<?php\nnamespace App\\Two;\nclass Svc { function go() {} }\n"),
                ("three.php", "<?php\nnamespace App\\One;\nclass Dup { function go() {} }\nclass Caller { function run() { Dup::go(); } }\n"),
            ],
        );

        let short = scanner.dump_chain("Svc::go");
        assert!(short.contains("ambiguous"), "{short}");
        assert!(short.contains("App\\One\\Svc::go (") && short.contains("one.php:L3"), "{short}");
        assert!(short.contains("App\\Two\\Svc::go (") && short.contains("two.php:L3"), "{short}");
        assert_eq!(steps(&scanner.dump_chain("App\\One\\Svc::go")), ["App\\One\\Svc::go"]);

        let dup = scanner.dump_chain("App\\One\\Dup::go");
        assert!(dup.contains("ambiguous") && dup.contains("one.php:L4") && dup.contains("three.php:L3"), "{dup}");

        let caller = scanner.dump_chain("Caller::run");
        assert_eq!(steps(&caller), ["App\\One\\Caller::run"]);
        assert!(caller.contains("`Dup::go()`: ambiguous") && caller.contains("one.php:L4") && caller.contains("three.php:L3"), "{caller}");
    }

    #[test]
    fn resolves_traits_and_parents() {
        let scanner = scan(
            "traits",
            &[(
                "t.php",
                r#"<?php
trait Hello { function say() {} function hi() {} }
trait World { function say() {} function hi() {} }
class Base { function up() {} function say() {} }
class Child extends Base {
    use Hello, World { Hello::say insteadof World; World::say as worldSay; }
    function run() { $this->say(); $this->worldSay(); self::up(); static::UP(); parent::say(); $this?->say(); }
    function bad() { $this->hi(); $obj->x(); $this->$dyn(); parent::nope(); }
}
class Own extends Base { use Hello; function say() {} function go() { $this->say(); $this->up(); } }
class Mid extends Base { use Hello; function go() { $this->say(); } }
"#,
            )],
        );

        assert_eq!(
            steps(&scanner.dump_chain("Child::run")),
            ["Child::run", "Hello::say", "World::say", "Base::up", "Base::say"]
        );

        let bad = scanner.dump_chain("Child::bad");
        assert_eq!(steps(&bad), ["Child::bad"]);
        let n = notes(&bad);
        assert_eq!(n.len(), 3, "dynamic $this->$dyn() must not be reported: {bad}");
        assert!(n[0].contains("`$this->hi()`: trait conflict") && n[0].contains("Hello::hi (") && n[0].contains("World::hi ("), "{bad}");
        assert!(n[1].contains("`$obj->x()`: unresolved: receiver type unknown"), "{bad}");
        assert!(n[2].contains("`parent::nope()`: unresolved"), "{bad}");

        assert_eq!(steps(&scanner.dump_chain("Own::go")), ["Own::go", "Own::say", "Base::up"]);
        assert_eq!(steps(&scanner.dump_chain("Mid::go")), ["Mid::go", "Hello::say"], "trait must win over parent");
    }
}
