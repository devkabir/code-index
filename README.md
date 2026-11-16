# cs

A small code search engine for AI agents. It strips comments and whitespace from source files, indexes them with [tantivy](https://github.com/quickwit-oss/tantivy), and returns matching lines in a compact text format.

## Build

```bash
cargo build --release
```

The binary is `target/release/cs`.

## Use

Run `cs` from the directory you want to search. It indexes and searches the current directory, and stores the index in `./.code-index`.

```bash
cs index            # build or rebuild the index
cs "add_action"     # search
cs mcp              # run as an MCP server on stdio
```

The first search builds the index if none exists. After that, the index is only updated by `cs index`.

## Output

`# <path>` names a file. `L<line>:<code>` is a matching line with its line number in the original file.

```text
# src/Controller/Firewall.php
L42:public function __construct(){$this->register_routes();}
L89:add_action('init',[$this,'ip_lockout']);
```

A search returns at most 10 files and 8 lines per file.

## Ignore rules

`cs` reads `.csignore` files in the searched directory tree. They use `.gitignore` syntax. `cs` does not read `.gitignore`, and it does not skip hidden files, so list `.git/` and `.code-index/` yourself:

```gitignore
.git/
.code-index/
target/
node_modules/
*.min.js
```

## Queries

- Terms separated by spaces are OR'd.
- One identifier such as `register_routes` or `ip-lockout` matches as a whole.
- A query that contains `:` returns nothing. Search `Foo bar` instead of `Foo::bar`.

## MCP tools

| Tool | Arguments | Result |
| :--- | :--- | :--- |
| `search_code` | `query` (string) | Same text as the CLI search |
| `index_code` | none | Rebuilds the index |

## Docker

```bash
docker build -t cs .
docker run --rm -v "$PWD:/workspace" cs index
docker run --rm -v "$PWD:/workspace" cs "add_action"
```

Run `docker build` in this folder. Run `docker run` in the directory you want to search.

With Compose, run these in the directory you want to search and point `-f` at this folder:

```bash
docker compose -f cs/docker-compose.yml run --rm cs index
docker compose -f cs/docker-compose.yml run --rm cs "add_action"
```

## Limits

The minifier does not parse strings. It cuts a line at `//`, also inside a URL, and at a `#` that starts a line or follows a space. Read the real file before you quote or edit code from a result.
