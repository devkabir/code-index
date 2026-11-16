use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::{env, fs, io::{self, BufRead, Write}, ops::Bound, path::Path};
use tantivy::{collector::TopDocs, doc, query::{BooleanQuery, Occur, Query, QueryParser, RangeQuery}, schema::{Value as _, *}, Index, ReloadPolicy, Term};

const INDEX_DIR: &str = ".code-index";

fn compact_code(src: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for (idx, raw_line) in src.lines().enumerate() {
        let mut line = Vec::new();
        let bytes = raw_line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if in_block {
                if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    in_block = false;
                    i += 2;
                } else {
                    i += 1;
                }
            } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                in_block = true;
                i += 2;
            } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                break;
            } else if bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
                break;
            } else {
                line.push(bytes[i]);
                i += 1;
            }
        }
        let line = String::from_utf8_lossy(&line);
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let minified = collapsed
            .replace(" { ", "{")
            .replace(" {", "{")
            .replace("{ ", "{")
            .replace(" } ", "}")
            .replace(" }", "}")
            .replace("; ", ";")
            .replace(", ", ",")
            .replace(" = ", "=")
            .replace(" ( ", "(")
            .replace(" (", "(")
            .replace("( ", "(")
            .replace(" )", ")");
        if !minified.is_empty() {
            out.push_str(&format!("L{}:{}\n", idx + 1, minified));
        }
    }
    out
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
        .build();
    for entry in walker.flatten() {
        let p = entry.path();
        if p.is_file() {
            if let Ok(text) = fs::read_to_string(p) {
                let compacted = compact_code(&text);
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

fn search(query_str: &str, folder: &str, limit: usize) -> tantivy::Result<String> {
    if !Path::new(INDEX_DIR).join("meta.json").exists() {
        build_index()?;
    }
    let (index, f_path, f_body) = open_or_create_index()?;
    let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(&index, vec![f_path, f_body]);
    let (q, _) = qp.parse_query_lenient(query_str);
    let folder = folder.trim_start_matches("./").trim_end_matches('/');
    let q: Box<dyn Query> = if folder.is_empty() {
        q
    } else {
        let in_folder = RangeQuery::new(
            Bound::Included(Term::from_field_text(f_path, &format!("{folder}/"))),
            Bound::Excluded(Term::from_field_text(f_path, &format!("{folder}/\u{10ffff}"))),
        );
        Box::new(BooleanQuery::new(vec![(Occur::Must, q), (Occur::Must, Box::new(in_folder))]))
    };
    let top_docs = searcher.search(&q, &TopDocs::with_limit(limit))?;
    let q_terms: Vec<String> = query_str
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();

    let mut out = String::new();
    for (_score, addr) in top_docs {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let path = doc.get_first(f_path).and_then(|v| v.as_str()).unwrap_or("");
        let body = doc.get_first(f_body).and_then(|v| v.as_str()).unwrap_or("");
        let matches: Vec<&str> = body
            .lines()
            .filter(|line| {
                let l = line.to_lowercase();
                q_terms.iter().any(|t| l.contains(t))
            })
            .take(8)
            .collect();
        if !matches.is_empty() {
            out.push_str(&format!("# {path}\n{}\n", matches.join("\n")));
        } else {
            out.push_str(&format!("# {path}\n"));
        }
    }
    Ok(out)
}

fn run_mcp() {
    let stdin = io::stdin();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req.get("id").cloned();
        let resp = match req["method"].as_str().unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "cs", "version": "0.1.0" }
            }),
            "tools/list" => json!({
                "tools": [
                    {
                        "name": "search_code",
                        "description": "Search minified code index (.code-index); returns compact # file and L#:code lines",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" },
                                "folder": { "type": "string", "description": "Only return files under this folder, relative to the index root" }
                            },
                            "required": ["query"]
                        }
                    },
                    {
                        "name": "index_code",
                        "description": "Rebuild minified .code-index using .csignore",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }),
            "tools/call" => match req["params"]["name"].as_str().unwrap_or("") {
                "index_code" => {
                    let count = build_index().unwrap_or(0);
                    json!({ "content": [{ "type": "text", "text": format!("Indexed {count} files") }] })
                }
                _ => {
                    let q = req["params"]["arguments"]["query"].as_str().unwrap_or("");
                    let folder = req["params"]["arguments"]["folder"].as_str().unwrap_or("");
                    let hits = search(q, folder, 10).unwrap_or_default();
                    json!({ "content": [{ "type": "text", "text": hits }] })
                }
            },
            _ => continue,
        };
        if let Some(id) = id {
            let _ = writeln!(io::stdout(), "{}", json!({"jsonrpc": "2.0", "id": id, "result": resp}));
            let _ = io::stdout().flush();
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("index") => println!("Indexed {} files into {}", build_index().unwrap(), INDEX_DIR),
        Some("mcp") => run_mcp(),
        Some(q) => print!("{}", search(q, args.get(2).map_or("", |s| s.as_str()), 10).unwrap()),
        None => eprintln!("Usage: cs <index | mcp | \"search query\" [folder]>"),
    }
}
