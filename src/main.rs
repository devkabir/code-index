pub mod chain;

use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::{env, fs, io::{self, BufRead, Write}, ops::Bound, path::Path};
use tantivy::{collector::TopDocs, doc, query::{BooleanQuery, Occur, Query, QueryParser, RangeQuery, RegexQuery}, schema::{Value as _, *}, Index, ReloadPolicy, Term, TantivyError};

const INDEX_DIR: &str = ".code-index";
const MAX_LINES: usize = 8;
const MAX_BLOCK: usize = 80;

fn compact_code(src: &str, ext: &str) -> String {
    let slash = matches!(ext, "php" | "js" | "jsx" | "mjs" | "ts" | "tsx" | "rs" | "c" | "h" | "cpp" | "java" | "go" | "scss" | "less");
    let hash = matches!(ext, "php" | "py" | "sh" | "rb" | "yml" | "yaml" | "toml");
    let mut out = String::new();
    let mut in_block = false;
    for (idx, raw_line) in src.lines().enumerate() {
        let b = raw_line.as_bytes();
        let mut line = Vec::new();
        // Context: comment markers inside quotes are code (URLs, globs, regex), so quotes are tracked per line.
        let mut quote = None;
        let mut i = 0;
        while i < b.len() {
            let next = b.get(i + 1).copied();
            if in_block {
                if b[i] == b'*' && next == Some(b'/') {
                    in_block = false;
                    i += 1;
                }
            } else if let Some(q) = quote {
                line.push(b[i]);
                if b[i] == b'\\' {
                    if let Some(n) = next {
                        line.push(n);
                        i += 1;
                    }
                } else if b[i] == q {
                    quote = None;
                }
            } else if slash && b[i] == b'/' && next == Some(b'*') {
                in_block = true;
                i += 1;
            } else if slash && b[i] == b'/' && next == Some(b'/') {
                break;
            } else if hash && b[i] == b'#' && next != Some(b'[') && (i == 0 || b[i - 1].is_ascii_whitespace()) {
                break;
            } else {
                if matches!(b[i], b'\'' | b'"' | b'`') {
                    quote = Some(b[i]);
                }
                line.push(b[i]);
            }
            i += 1;
        }
        let line = String::from_utf8_lossy(&line);
        let line = line.trim();
        if !line.is_empty() {
            out.push_str(&format!("L{}:{line}\n", idx + 1));
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn has_word(line: &str, term: &str) -> bool {
    line.match_indices(term).any(|(i, _)| {
        !line[..i].chars().next_back().is_some_and(is_ident) && !line[i + term.len()..].chars().next().is_some_and(is_ident)
    })
}

fn is_def(line: &str, term: &str) -> bool {
    ["function ", "function &", "class ", "interface ", "trait ", "enum ", "struct ", "fn ", "def "]
        .iter()
        .any(|kw| has_word(line, &format!("{kw}{term}")))
}

fn block_end(lines: &[&str], start: usize) -> usize {
    let (mut depth, mut seen) = (0i32, false);
    for (n, l) in lines.iter().enumerate().skip(start).take(MAX_BLOCK) {
        depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
        seen |= l.contains('{');
        if depth <= 0 && (seen || l.ends_with(';') || n - start >= 2) {
            return n + 1;
        }
    }
    (start + MAX_BLOCK).min(lines.len())
}

fn open_or_create_index() -> tantivy::Result<(Index, Field, Field)> {
    let mut sb = Schema::builder();
    let path = sb.add_text_field("path", STRING | STORED);
    let body = sb.add_text_field("body", TEXT | STORED);
    let schema = sb.build();
    fs::create_dir_all(INDEX_DIR)?;
    let dir = tantivy::directory::MmapDirectory::open(INDEX_DIR)?;
    let index = Index::open_or_create(dir, schema)?;
    Ok((index, path, body))
}

fn build_index() -> tantivy::Result<usize> {
    let (index, f_path, f_body) = open_or_create_index()?;
    let mut writer = index.writer(50_000_000)?;
    writer.delete_all_documents()?;
    let mut count = 0;
    let walker = WalkBuilder::new(".")
        .standard_filters(false)
        .add_custom_ignore_filename(".csignore")
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | INDEX_DIR)))
        .build();
    for entry in walker.flatten() {
        let p = entry.path();
        if p.is_file() {
            if let Ok(text) = fs::read_to_string(p) {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                let compacted = compact_code(&text, ext);
                if !compacted.is_empty() {
                    let rel = p.strip_prefix("./").unwrap_or(p).to_string_lossy();
                    writer.add_document(doc!(f_path => rel.as_ref(), f_body => compacted))?;
                    count += 1;
                }
            }
        }
    }
    writer.commit()?;
    Ok(count)
}

fn path_filters(f_path: Field, folder: &str, ext: &str) -> tantivy::Result<Vec<(Occur, Box<dyn Query>)>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    let folder = folder.trim_start_matches("./").trim_end_matches('/');
    if !folder.is_empty() {
        clauses.push((Occur::Must, Box::new(RangeQuery::new(
            Bound::Included(Term::from_field_text(f_path, &format!("{folder}/"))),
            Bound::Excluded(Term::from_field_text(f_path, &format!("{folder}/\u{10ffff}"))),
        ))));
    }
    if !ext.is_empty() {
        // Context: ext is spliced into a regex, so only letters, digits and | are allowed; tantivy-fst regex matches the whole path term.
        if !ext.chars().all(|c| c.is_ascii_alphanumeric() || c == '|') {
            return Err(TantivyError::InvalidArgument("ext must look like php or js|jsx".into()));
        }
        clauses.push((Occur::Must, Box::new(RegexQuery::from_pattern(&format!(r".*\.({ext})"), f_path)?)));
    }
    Ok(clauses)
}

fn search(query_str: &str, folder: &str, ext: &str, refs: bool, limit: usize) -> tantivy::Result<String> {
    let meta = Path::new(INDEX_DIR).join("meta.json");
    if !meta.exists() {
        build_index()?;
    }
    let indexed_at = fs::metadata(&meta)?.modified()?;
    let q_terms: Vec<String> = query_str
        .split(|c: char| !is_ident(c))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();
    if q_terms.is_empty() {
        return Ok("NO MATCHES: query has no identifier characters\n".into());
    }
    let (index, f_path, f_body) = open_or_create_index()?;
    let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
    let searcher = reader.searcher();
    let mut qp = QueryParser::for_index(&index, vec![f_body]);
    qp.set_conjunction_by_default();
    let (q, _) = qp.parse_query_lenient(&q_terms.join(" "));
    let mut clauses = path_filters(f_path, folder, ext)?;
    clauses.push((Occur::Must, q));
    let q = BooleanQuery::new(clauses);
    let fetch = limit * 5;
    let top_docs = searcher.search(&q, &TopDocs::with_limit(fetch))?;
    let capped = top_docs.len() == fetch;

    let mut hits: Vec<(bool, String)> = Vec::new();
    for (_score, addr) in top_docs {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let path = doc.get_first(f_path).and_then(|v| v.as_str()).unwrap_or("");
        let body = doc.get_first(f_body).and_then(|v| v.as_str()).unwrap_or("");
        let lines: Vec<&str> = body.lines().collect();
        let lower: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
        let all: Vec<usize> = (0..lines.len()).filter(|&i| q_terms.iter().all(|t| has_word(&lower[i], t))).collect();
        let idx = if all.is_empty() {
            (0..lines.len()).filter(|&i| q_terms.iter().any(|t| has_word(&lower[i], t))).collect()
        } else {
            all
        };
        // Context: tokenizer matches wp_logout to wp-logout too; files with no whole-identifier line are dropped.
        if idx.is_empty() {
            continue;
        }
        let (mut defs, mut plain, mut covered) = (String::new(), Vec::new(), 0);
        for i in idx {
            if i < covered {
                continue;
            }
            // Context: refs mode prints a definition as its one signature line instead of the full block.
            if !refs && q_terms.iter().any(|t| is_def(&lower[i], t)) {
                let end = block_end(&lines, i);
                defs.push_str(&format!("[def]\n{}\n", lines[i..end].join("\n")));
                if end - i == MAX_BLOCK {
                    defs.push_str(&format!("[cut at {MAX_BLOCK} lines: Read the file for the rest]\n"));
                }
                covered = end;
            } else {
                plain.push(lines[i]);
            }
        }
        // Context: stored line numbers are only valid for files unchanged since the last index_code.
        let stale = fs::metadata(path).and_then(|m| m.modified()).map_or(true, |t| t > indexed_at);
        let mut out = format!("# {path}{}\n{defs}", if stale { " [STALE: changed or deleted since index; run index_code]" } else { "" });
        for l in plain.iter().take(MAX_LINES) {
            out.push_str(&format!("{l}\n"));
        }
        if plain.len() > MAX_LINES {
            out.push_str(&format!("[+{} more lines]\n", plain.len() - MAX_LINES));
        }
        hits.push((!defs.is_empty(), out));
    }
    if hits.is_empty() {
        return Ok(format!("NO MATCHES for whole identifier(s): {}\n", q_terms.join(" ")));
    }
    hits.sort_by_key(|(def, _)| !def);
    let mut out: String = hits.iter().take(limit).map(|(_, s)| s.as_str()).collect();
    if hits.len() > limit || capped {
        out.push_str(&format!("[showing {limit} of {}{} files; narrow with folder]\n", hits.len(), if capped { "+" } else { "" }));
    }
    Ok(out)
}

fn reply(id: Value, body: Value) {
    let mut msg = json!({"jsonrpc": "2.0", "id": id});
    msg.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    let _ = writeln!(io::stdout(), "{msg}");
    let _ = io::stdout().flush();
}

fn run_mcp() {
    let stdin = io::stdin();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = req.get("id").cloned() else { continue };
        let result = match req["method"].as_str().unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "cs", "version": "0.2.0" }
            }),
            "tools/list" => json!({
                "tools": [
                    {
                        "name": "search_code",
                        "description": "Whole-identifier search (all terms required) over a comment-stripped, unindented copy of the code. Output: `# file`, then `[def]` full definition blocks (max 80 lines), then `L#:code` hit lines (max 8/file). Markers: `[+N more lines]`, `[showing N of M files]`, `[STALE]`, `NO MATCHES`. Lines lack comments and indentation: Read the file before quoting, editing, or claiming something is absent.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" },
                                "folder": { "type": "string", "description": "Folder to search, relative to the index root. Plugin code: <plugin>/src. Libraries: <plugin>/vendor. \"\" = whole repo" },
                                "ext": { "type": "string", "description": "File extensions, | separated, e.g. php or js|jsx. \"\" = all" },
                                "refs": { "type": "boolean", "description": "true = show definitions as one line (use when listing references)" }
                            },
                            "required": ["query", "folder"]
                        }
                    },
                    {
                        "name": "index_code",
                        "description": "Rebuild .code-index using .csignore",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }),
            "tools/call" => {
                let a = &req["params"]["arguments"];
                let r = match req["params"]["name"].as_str().unwrap_or("") {
                    "index_code" => build_index().map(|c| format!("Indexed {c} files")),
                    "search_code" => search(
                        a["query"].as_str().unwrap_or(""),
                        a["folder"].as_str().unwrap_or(""),
                        a["ext"].as_str().unwrap_or(""),
                        a["refs"].as_bool().unwrap_or(false),
                        10,
                    ),
                    n => Err(TantivyError::InvalidArgument(format!("unknown tool: {n}"))),
                };
                match r {
                    Ok(t) => json!({ "content": [{ "type": "text", "text": t }] }),
                    Err(e) => json!({ "content": [{ "type": "text", "text": format!("ERROR: {e}") }], "isError": true }),
                }
            }
            m => {
                reply(id, json!({ "error": { "code": -32601, "message": format!("Method not found: {m}") } }));
                continue;
            }
        };
        reply(id, json!({ "result": result }));
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("index") => println!("Indexed {} files into {}", build_index().unwrap(), INDEX_DIR),
        Some("chain") => {
            let target = args.get(2).map(|s| s.as_str()).unwrap_or("");
            if target.is_empty() {
                eprintln!("Usage: cs chain <method_or_class::method> [path]");
                return;
            }
            let scan_path = args.get(3).map(|s| s.as_str()).unwrap_or(".");
            let mut scanner = chain::CodebaseChainScanner::new();
            scanner.scan_dir(Path::new(scan_path));
            print!("{}", scanner.dump_chain(target));
        }
        Some("mcp") => run_mcp(),
        Some(q) => print!("{}", search(q, args.get(2).map_or("", |s| s.as_str()), args.get(3).map_or("", |s| s.as_str()), false, 10).unwrap()),
        None => eprintln!("Usage: cs <index | chain <method> [path] | mcp | \"search query\" [folder] [ext]>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check() {
        let php = "<?php\nfunction wp_logout() {\n  $u = 'https://x.com'; // c\n  # c\n  #[Attr]\n  if ($a) { $b = '/*.php'; }\n}\nwp_logout_url();\n";
        let c = compact_code(php, "php");
        assert!(c.contains("L3:$u = 'https://x.com';\n"));
        assert!(!c.contains("L4:"));
        assert!(c.contains("L5:#[Attr]") && c.contains("L6:if ($a) { $b = '/*.php'; }"));
        let lines: Vec<&str> = c.lines().collect();
        let lower = lines[1].to_lowercase();
        assert!(is_def(&lower, "wp_logout"));
        assert_eq!(block_end(&lines, 1), 6);
        assert!(!has_word("l8:wp_logout_url();", "wp_logout"));
        assert!(has_word("add_action('wp_logout', 'x')", "wp_logout"));
        assert!(compact_code("#include <x>\n", "c").contains("#include"));
    }

    #[test]
    fn path_filter() {
        let mut sb = Schema::builder();
        let p = sb.add_text_field("path", STRING | STORED);
        let index = Index::create_in_ram(sb.build());
        let mut w = index.writer(50_000_000).unwrap();
        for f in ["src/a.js", "src/a.jsx", "src/a.json", "src/a.jsx.map", "src/a.php", "vendor/b.js"] {
            w.add_document(doc!(p => f)).unwrap();
        }
        w.commit().unwrap();
        let s = index.reader().unwrap().searcher();
        let n = |folder: &str, ext: &str| s.search(&BooleanQuery::new(path_filters(p, folder, ext).unwrap()), &tantivy::collector::Count).unwrap();
        assert_eq!(n("src", "js|jsx"), 2);
        assert_eq!(n("", "js"), 2);
        assert_eq!(n("src", "php"), 1);
        assert!(path_filters(p, "", ".*").is_err());
    }
}
